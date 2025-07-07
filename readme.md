# telegram-images-bot

一个telegram图片下载机器人，采用rust编写。

## 使用方法
创建`.env`文件或在环境变量中添加`TG_BOT_TOKEN=[your token is here]`，用你的token替换掉`[your token is here]`。

使用`cargo b --release`编译程序，运行`./target/release/telegram-images-bot`

或者使用`sudo docker-compose up -d`直接在源码目录启动服务。