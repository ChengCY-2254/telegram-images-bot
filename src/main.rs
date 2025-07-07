use reqwest::Client;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{InputFile, Me};
use tokio::sync::Mutex;
use uuid::Uuid;
use zip::ZipWriter;
use zip::write::FileOptions;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    dotenv::dotenv().ok();

    log::info!("开始链接telegram数据中心");
    let bot = Config::from_env().into_bot();
    log::info!("链接成功");

    let client = Client::new();
    let state: AppState = Arc::new(Mutex::new(HashMap::new()));
    let handler = dptree::entry().branch(Update::filter_message().endpoint(handle_message));

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
    is_collecting: bool,
    messages: Vec<Message>,
}

/// 消息处理函数
async fn handle_message(
    bot: Bot,
    msg: Message,
    client: Client,
    state: AppState,
    me: Me,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let chat_id = msg.chat.id;
    let bot = Arc::new(bot);
    if let Some(text) = msg.text() {
        let bot_name = me.username();
        match text {
            "/start" | "/help" | "?" => {
                bot.send_message(chat_id, "你好！我是图片下载机器人。\n\n使用`/StartCollect`开始收集图片，使用`/StopCollect`停止并打包下载".to_string()).await?;
                return Ok(());
            }
            cmd if cmd == "/StartCollect" || cmd == format!("/StartCollect@{}", bot_name) => {
                start_collecting(Arc::clone(&bot), chat_id, state).await?;
                return Ok(());
            }
            cmd if cmd == "/StopCollect" || cmd == format!("/StopCollect@{}", bot_name) => {
                tokio::spawn(stop_collecting_and_process(
                    Arc::clone(&bot),
                    chat_id,
                    state,
                    client,
                ));
                return Ok(());
            }
            _ => {
                //其它内容，不解析
            }
        }
    }

    let mut state_guard = state.lock().await;
    let user_state = state_guard.entry(chat_id).or_default();

    if user_state.is_collecting {
        log::trace!("用户 {} 有一个收集会话 {}", chat_id, msg.id);
        user_state.messages.push(msg.clone());
    }

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
        "✅收集已开始，请发送图片或包含图片的消息。完成后，发送`/StopCollect`以结束收集",
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
    let messages_to_process = {
        let mut state_guard = state.lock().await;
        let user_state = state_guard.entry(chat_id).or_default();

        if !user_state.is_collecting {
            bot.send_message(chat_id, "🤔 你还没有开始收集，请先发送 /StartCollect。")
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
        std::mem::take(&mut user_state.messages)
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
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M");
    let zip_filename = format!("images_{}_{}.zip", now, chat_id.0);
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
