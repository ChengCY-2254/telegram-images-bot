use reqwest::Client;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::InputFile;
use teloxide::utils::command::BotCommands;
use tokio::sync::Mutex;
use uuid::Uuid;
use zip::ZipWriter;
use zip::write::FileOptions;

pub const VERSION: &str = include_str!(concat!(env!("OUT_DIR"), "/VERSION"));

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    dotenv::dotenv().ok();

    log::info!("开始链接telegram数据中心");
    let bot = Config::from_env().into_bot();
    log::info!("链接成功");

    log::info!("开始注册命令");

    if let Err(why) = bot.set_my_commands(Command::bot_commands()).await {
        log::error!("无法注册命令: {}", why);
    } else {
        log::info!("命令注册成功");
    }

    let client = Client::new();
    let state: AppState = Arc::new(Mutex::new(HashMap::new()));

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(command_handler),
        )
        .branch(Update::filter_message().endpoint(handle_message));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![client, state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

#[derive(Debug)]
#[repr(transparent)]
struct Config {
    bot_token: String,
}

impl Config {
    fn from_env() -> Self {
        Config {
            bot_token: std::env::var("TG_BOT_TOKEN").expect("TG_BOT_TOKEN must be set"),
        }
    }
    fn into_bot(self) -> Bot {
        Bot::new(self.bot_token)
    }
}

type AppState = Arc<Mutex<HashMap<ChatId, UserState>>>;

#[derive(Debug, Default)]
struct UserState {
    /// 是否是收集模式
    is_collecting: bool,
    /// 是否设置文件名
    is_set_file_name: bool,
    /// 收集的消息
    messages: Vec<Message>,
    /// 打包的文件名
    file_name: Option<String>,
}

#[derive(BotCommands, Clone)]
//如果不采用小写，telegram就无法注册命令
#[command(rename_rule = "lowercase")]
enum Command {
    #[command(description = "显示此帮助信息")]
    Start,
    #[command(description = "显示此帮助信息")]
    Help,
    #[command(description = "开始收集图片信息")]
    StartCollect,
    #[command(description = "停止收集并打包下载所有图片")]
    StopCollect,
    #[command(description = "显示程序版本")]
    Version,
    #[command(description = "设置zip名称")]
    FileName,
}

/// 消息处理函数
/// 处理用户的收集消息，如果用户没有开启收集模式，则忽略。
async fn handle_message(
    bot: Bot,
    msg: Message,
    state: AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let chat_id = msg.chat.id;

    let mut state_guard = state.lock().await;
    let user_state = state_guard.entry(chat_id).or_default();

    if user_state.is_collecting {
        log::trace!("用户 {} 有一个收集会话 {}", chat_id, msg.id);
        user_state.messages.push(msg.clone());
    } else if user_state.is_set_file_name {
        log::trace!("用户 {} 有一个设置文件名会话 {}", chat_id, msg.id);
        let file_name = msg.text().unwrap_or_default().to_string();

        if file_name.is_empty() {
            bot.send_message(chat_id, "❌ 文件名不能为空").await?;
            return Ok(());
        }

        user_state.file_name = Some(file_name);
        bot.send_message(
            chat_id,
            format!(
                "✅已设置文件名为 {}.zip",
                user_state.file_name.as_ref().unwrap()
            ),
        )
        .await?;
        // 停止设置文件名会话
        user_state.is_set_file_name = false;
    }

    Ok(())
}

/// 命令处理函数
async fn command_handler(
    bot: Bot,
    msg: Message,
    cmd: Command,
    client: Client,
    state: AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let chat_id = msg.chat.id;
    let bot = Arc::new(bot);

    match cmd {
        Command::Start | Command::Help => {
            bot.send_message(chat_id, "你好！我是图片下载机器人。\n\n/startcollect - 开始收集图片\n/stopcollect - 停止并打包下载\n/filename - 设置文件名称").await?;
        }
        Command::StartCollect => {
            start_collecting(bot, chat_id, state).await?;
        }
        Command::StopCollect => {
            // 耗时任务放入后台执行
            tokio::spawn(stop_collecting_and_process(bot, chat_id, state, client));
        }
        Command::Version => {
            bot.send_message(chat_id, format!("当前版本：{}", VERSION))
                .await?;
        }
        Command::FileName => {
            start_set_file_name(bot, chat_id, state).await?;
        }
    }

    Ok(())
}

async fn start_set_file_name(bot: Arc<Bot>, chat: ChatId, state: AppState)->Result<(), Box<dyn std::error::Error + Send + Sync>> {
    bot.send_message(chat, "请将文件名发送给我，我会将其设置为压缩包名")
        .await?;
    let mut state_guard = state.lock().await;
    let user_state = state_guard.entry(chat).or_default();
    user_state.is_set_file_name = true;

    Ok(())
}

async fn start_collecting(
    bot: Arc<Bot>,
    chat_id: ChatId,
    state: AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut state_guard = state.lock().await;
    let user_state = state_guard.entry(chat_id).or_default();

    user_state.is_collecting = true;
    user_state.messages.clear();

    log::info!("会话 {} 开启了一个收集任务", chat_id);
    bot.send_message(
        chat_id,
        "✅收集已开始，请发送图片或包含图片的消息。完成后，发送/stopcollect以结束收集",
    )
    .await?;
    Ok(())
}

async fn stop_collecting_and_process(
    bot: Arc<Bot>,
    chat_id: ChatId,
    state: AppState,
    client: Client,
) {
    if let Err(e) = process_inner(Arc::clone(&bot), chat_id, state.clone(), client.clone()).await {
        log::error!("Error processing for chat {}: {}", chat_id, e);
        let _ = bot
            .send_message(chat_id, format!("❌ 处理失败: {}", e))
            .await;
    }
}

async fn process_inner(
    bot: Arc<Bot>,
    chat_id: ChatId,
    state: AppState,
    client: Client,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (messages_to_process, file_name) = {
        let mut state_guard = state.lock().await;
        let user_state = state_guard.entry(chat_id).or_default();

        if !user_state.is_collecting {
            bot.send_message(chat_id, "🤔 你还没有开始收集，请先发送 /startcollect。")
                .await?;
            return Ok(());
        }

        user_state.is_collecting = false;
        log::info!(
            "Stopped collecting for chat {}. Processing {} messages.",
            chat_id,
            user_state.messages.len()
        );

        // 克隆消息列表并释放锁
        let messages = std::mem::take(&mut user_state.messages);
        let file_name = user_state.file_name.take();
        (messages, file_name)
    };

    if messages_to_process.is_empty() {
        bot.send_message(chat_id, "ℹ️ 你没有发送任何消息，无需处理。")
            .await?;
        return Ok(());
    }

    bot.send_message(chat_id, "⏳ 正在处理，请稍候...").await?;

    let token = bot.token();
    let mut photo_urls = Vec::new();

    // 1. 提取所有图片的下载链接
    for msg in &messages_to_process {
        if let Some(photos) = msg.photo() {
            // 获取最高分辨率的图片
            if let Some(largest_photo) = photos.iter().max_by_key(|p| p.height * p.width) {
                let file = bot.get_file(largest_photo.file.id.clone()).await?;
                let url = format!("https://api.telegram.org/file/bot{}/{}", token, file.path);
                photo_urls.push(url);
            }
        }
    }

    if photo_urls.is_empty() {
        bot.send_message(chat_id, "🤷‍♀️ 在你发送的消息中没有找到任何图片。")
            .await?;
        return Ok(());
    }

    // 2. 创建临时目录并下载图片
    let temp_dir_name = format!("temp_{}_{}", chat_id.0, Uuid::new_v4());
    let temp_dir = PathBuf::from(&temp_dir_name);
    let zip_filename = if file_name.is_none() {
        let now = chrono::Local::now().format("%Y-%m-%d:%H:%M");
        format!("images_{}_{}.zip", now, chat_id.0)
    } else {
        format!("{}.zip", file_name.unwrap())
    };
    let zip_path = PathBuf::from(&zip_filename);

    tokio::fs::create_dir_all(&temp_dir).await?;

    {
        let mut downloads = Vec::new();

        for (i, url) in photo_urls.clone().into_iter().enumerate() {
            let client = client.clone();
            let temp_dir_cloned = temp_dir.clone(); // 克隆 temp_dir 所有权到异步块内
            downloads.push(tokio::spawn(async move {
                let response = client.get(url).send().await.unwrap();
                let bytes = response.bytes().await.unwrap();
                let file_path = temp_dir_cloned.join(format!("image_{}.jpg", i + 1));
                tokio::fs::write(file_path, &bytes).await.unwrap();
            }));
        }

        futures::future::join_all(downloads).await;
    }

    log::info!(
        "Downloaded {} photos to {}",
        photo_urls.len(),
        temp_dir_name
    );

    create_zip(&temp_dir, &zip_path)?;
    log::info!("Created zip file: {}", zip_filename);

    // 4. 发送 ZIP 文件
    bot.send_message(
        chat_id,
        format!(
            "✅ 处理完成！共下载 {} 张图片，正在发送压缩包...",
            photo_urls.len()
        ),
    )
    .await?;
    bot.send_document(chat_id, InputFile::file(&zip_path))
        .await?;
    log::info!("Sent zip file to chat {}", chat_id);

    // 5. 清理临时文件和目录
    tokio::fs::remove_dir_all(&temp_dir).await?;
    tokio::fs::remove_file(&zip_path).await?;
    log::info!("Cleaned up temporary files for chat {}", chat_id);

    Ok(())
}

fn create_zip(src_dir: &Path, dst_file: &Path) -> zip::result::ZipResult<()> {
    let file = File::create(dst_file)?;
    let mut zip = ZipWriter::new(file);
    let options = FileOptions::<()>::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);

    let mut buffer = Vec::new();
    for entry in std::fs::read_dir(src_dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = path.file_name().unwrap().to_str().unwrap();

        if path.is_file() {
            zip.start_file(name, options)?;
            let mut f = File::open(path)?;
            f.read_to_end(&mut buffer)?;
            zip.write_all(&buffer)?;
            buffer.clear();
        }
    }
    zip.finish()?;
    Ok(())
}
