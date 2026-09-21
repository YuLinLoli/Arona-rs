//! `/攻略` 端到端自检：活动攻略 / 日程笔记 / 当期卡池 / 云端图片检索与数字回复。
//!
//! 需要联网访问 GameKee 与 arona 云端图片库，因此默认 ignore。
//! 运行: cargo test trainer_smoke -- --ignored --nocapture --test-threads=1

use arona::runtime::dispatcher::CommandContext;
use arona::runtime::message::{
    BoxFuture, ForwardMessage, MessageReceipt, MessageSegment, MessageSender, MessageTarget,
    OutgoingMessage,
};
use std::sync::{Arc, Mutex};

struct CaptureSender {
    sent: Mutex<Vec<(MessageTarget, OutgoingMessage)>>,
}

impl MessageSender for CaptureSender {
    fn send<'a>(
        &'a self,
        target: MessageTarget,
        message: OutgoingMessage,
    ) -> BoxFuture<'a, MessageReceipt> {
        Box::pin(async move {
            self.sent.lock().unwrap().push((target, message));
            MessageReceipt::new(Some(1))
        })
    }
}

fn setup() -> (
    Arc<arona::runtime::dispatcher::CommandDispatcher>,
    Arc<CaptureSender>,
    i64,
) {
    arona::runtime::paths::prepare();
    arona::runtime::services::set_data_root(arona::runtime::paths::data_root());
    arona::runtime::config::set_bot_id(10000);
    arona::runtime::config::set_end_with_sensei("老师".to_string());
    assert!(crate::db::start(), "数据库初始化失败");
    let config = arona::config::onebot::OneBotConfig {
        self_id: 10000,
        nickname: "Arona".to_string(),
        send_image_as_file: false,
        connections: std::collections::BTreeMap::new(),
    };
    crate::standalone::dispatcher::register_into_table(config);
    let dispatcher = Arc::new(arona::runtime::dispatcher::CommandDispatcher::new());
    let sender = Arc::new(CaptureSender {
        sent: Mutex::new(Vec::new()),
    });
    (dispatcher, sender, 10001)
}

fn context(sender: Arc<CaptureSender>, text: &str) -> Arc<CommandContext> {
    Arc::new(CommandContext {
        user_id: 10001,
        group_id: Some(900000001),
        text: text.to_string(),
        sender_name: Some("测试老师".to_string()),
        is_admin: true,
        sender_role: None,
        message_id: None,
        time: 0,
        quoted: None,
        segments: Vec::new(),
        sender,
    })
}

fn take_messages(sender: &CaptureSender) -> Vec<OutgoingMessage> {
    let mut guard = sender.sent.lock().unwrap();
    guard.drain(..).map(|(_, message)| message).collect()
}

fn image_files(message: &OutgoingMessage) -> Vec<String> {
    message
        .segments
        .iter()
        .filter_map(|segment| match segment {
            MessageSegment::Image {
                file: Some(file), ..
            } => Some(file.clone()),
            _ => None,
        })
        .collect()
}

fn forward_of(message: &OutgoingMessage) -> Option<(String, Vec<ForwardMessage>)> {
    message.segments.iter().find_map(|segment| match segment {
        MessageSegment::Forward {
            title, messages, ..
        } => Some((title.clone(), messages.clone())),
        _ => None,
    })
}

#[ignore = "联网自检, 运行: cargo test trainer_smoke -- --ignored --nocapture --test-threads=1"]
#[tokio::test]
async fn trainer_activity_guide_downloads_images() {
    let (dispatcher, sender, _) = setup();
    let handled = dispatcher
        .dispatch(context(sender.clone(), "/攻略 日服活动"))
        .await;
    assert!(handled, "调度器未识别 /攻略");
    let messages = take_messages(&sender);
    for message in &messages {
        println!("[日服活动] {:?}", message.segments);
    }
    let files: Vec<String> = messages.iter().flat_map(image_files).collect();
    assert!(!files.is_empty(), "未获取到日服活动攻略图片");
    for file in &files {
        assert!(std::path::Path::new(file).exists(), "图片不存在: {file}");
    }
    println!("[日服活动] 共 {} 张图片", files.len());
}

#[ignore = "联网自检, 运行: cargo test trainer_smoke -- --ignored --nocapture --test-threads=1"]
#[tokio::test]
async fn trainer_schedule_note_forwards_images() {
    let (dispatcher, sender, _) = setup();
    let handled = dispatcher
        .dispatch(context(sender.clone(), "/攻略 日程笔记"))
        .await;
    assert!(handled, "调度器未识别 /攻略");
    let messages = take_messages(&sender);
    for message in &messages {
        println!("[日程笔记] {:?}", message.segments);
    }
    let (title, nodes) = messages
        .iter()
        .find_map(forward_of)
        .expect("日程笔记未返回合并转发");
    assert_eq!(title, "日程笔记");
    assert!(nodes.len() > 1, "日程笔记节点数量异常: {}", nodes.len());
    for node in &nodes {
        println!("[日程笔记] 节点 {}: {:?}", node.name, node.content);
    }
}

#[ignore = "联网自检, 运行: cargo test trainer_smoke -- --ignored --nocapture --test-threads=1"]
#[tokio::test]
async fn trainer_gacha_pool_forward_contains_images() {
    let (dispatcher, sender, _) = setup();
    let handled = dispatcher
        .dispatch(context(sender.clone(), "/攻略 日服卡池"))
        .await;
    assert!(handled, "调度器未识别 /攻略");
    let messages = take_messages(&sender);
    for message in &messages {
        println!("[日服卡池] {:?}", message.segments);
    }
    if let Some((title, nodes)) = messages.iter().find_map(forward_of) {
        assert_eq!(title, "日服当期卡池");
        for node in &nodes {
            let has_text = node
                .content
                .iter()
                .any(|segment| matches!(segment, MessageSegment::Text(_)));
            assert!(has_text, "卡池节点缺少角色信息文本");
            for file in image_files(&OutgoingMessage {
                segments: node.content.clone(),
                revoke_after_millis: None,
            }) {
                assert!(std::path::Path::new(&file).exists(), "卡池图不存在: {file}");
                println!("[日服卡池] 图片: {file}");
            }
        }
    } else {
        panic!("日服卡池未返回合并转发: {messages:?}");
    }
}

#[ignore = "联网自检, 运行: cargo test trainer_smoke -- --ignored --nocapture --test-threads=1"]
#[tokio::test]
async fn trainer_cloud_image_and_numeric_reply() {
    let (dispatcher, sender, _) = setup();
    // 精确命中
    let handled = dispatcher
        .dispatch(context(sender.clone(), "/攻略 阿罗娜"))
        .await;
    assert!(handled);
    let messages = take_messages(&sender);
    let files: Vec<String> = messages.iter().flat_map(image_files).collect();
    assert!(!files.is_empty(), "未获取到「阿罗娜」图片");
    for file in &files {
        assert!(std::path::Path::new(file).exists(), "图片不存在: {file}");
        println!("[阿罗娜] {}", file);
    }

    // 模糊命中 -> 建议 -> 数字回复
    let handled = dispatcher
        .dispatch(context(sender.clone(), "/攻略 学生"))
        .await;
    assert!(handled);
    let messages = take_messages(&sender);
    let suggestion = messages
        .iter()
        .find_map(|message| match message.segments.first() {
            Some(MessageSegment::Text(text)) if text.contains("最接近的有") => {
                Some(text.clone())
            }
            _ => None,
        })
        .expect("模糊关键词未返回建议列表");
    println!("[模糊建议]\n{suggestion}");

    let reply_context = context(sender.clone(), "1");
    let reply = crate::standalone::commands::trainer::resolve_numeric_reply(reply_context).await;
    let reply = reply.expect("数字回复未命中待选建议");
    let files: Vec<String> = image_files(&reply);
    assert!(!files.is_empty(), "数字回复未返回图片");
    for file in &files {
        assert!(std::path::Path::new(file).exists(), "图片不存在: {file}");
        println!("[数字回复] {}", file);
    }
}

#[ignore = "联网自检, 运行: cargo test trainer_test -- --ignored --nocapture --test-threads=1"]
#[tokio::test]
async fn activity_command_returns_text() {
    let (dispatcher, sender, _) = setup();
    let handled = dispatcher
        .dispatch(context(sender.clone(), "/活动 日服"))
        .await;
    assert!(handled, "调度器未识别 /活动");
    let messages = take_messages(&sender);
    for message in &messages {
        println!("[活动] {:?}", message.segments);
    }
    assert!(!messages.is_empty(), "活动命令未返回任何消息");
    let mut image_file: Option<String> = None;
    for message in &messages {
        match message.segments.first() {
            Some(MessageSegment::Text(text)) => {
                assert!(
                    !text.contains("失败") && !text.contains("404"),
                    "活动命令返回异常: {text}"
                );
                println!("[活动] 文本: {text}");
            }
            Some(MessageSegment::Image {
                file: Some(file), ..
            }) => {
                assert!(std::path::Path::new(file).exists(), "活动图不存在: {file}");
                image_file = Some(file.clone());
                println!("[活动] 图片: {file}");
            }
            other => println!("[活动] 其它: {other:?}"),
        }
    }

    // 本地资源图片: 固定文件名 activity-<服务>.png; 再次调用命中本地图片, 文件时间不变(不重新渲染)
    let path = crate::image::activity::image_path(crate::entity::ServerLocale::JP);
    let Some(file) = image_file else {
        println!("[活动] 未生成图片(可能缺少中文字体), 跳过本地图片缓存校验");
        return;
    };
    assert_eq!(
        std::path::Path::new(&file),
        path.as_path(),
        "活动图应为本地资源图片 data/bluearchive/image/activity/activity-jp.png"
    );
    let before = std::fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .ok();
    let handled = dispatcher
        .dispatch(context(sender.clone(), "/活动 日服"))
        .await;
    assert!(handled, "调度器未识别 /活动");
    let cached: Vec<String> = take_messages(&sender)
        .iter()
        .flat_map(image_files)
        .collect();
    assert_eq!(
        cached,
        vec![path.to_string_lossy().to_string()],
        "二次调用应直接命中本地资源图片"
    );
    let after = std::fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .ok();
    assert_eq!(before, after, "命中本地图片时不应重新渲染");
    println!("[活动] 本地图片缓存命中: {}", path.display());
}
