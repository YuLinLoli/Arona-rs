//! `/十连` 端到端自检：走完整命令分发链路（服务守卫 -> 抽卡 -> 结果图渲染），
//! 结果图落盘到 `data/bluearchive/image/gacha/result/`。
//!
//! 需要联网拉取 kivo 学生数据/GameKee 当期卡池/学生头像，因此默认 ignore。
//! 运行: cargo test ten_pull -- --ignored --nocapture --test-threads=1

use arona::runtime::dispatcher::CommandContext;
use arona::runtime::message::{
    BoxFuture, MessageReceipt, MessageSegment, MessageSender, MessageTarget, OutgoingMessage,
};
use std::sync::{Arc, Mutex};

/// 记录命令回复的假发送器（独立模式真实实现是 OneBotMessageSender）
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
            MessageReceipt {
                message_id: Some(1),
            }
        })
    }
}

#[ignore = "端到端自检(需联网), 运行: cargo test ten_pull -- --ignored --nocapture --test-threads=1"]
#[tokio::test]
async fn ten_pull_outputs_image_into_standalone() {
    // 运行期依赖：数据目录 / 数据库 / 机器人配置
    let image_root = crate::image_dir();
    arona::runtime::services::set_data_root(arona::runtime::paths::data_root());
    arona::runtime::config::set_bot_id(10000);
    arona::runtime::config::set_end_with_sensei("老师".to_string());
    assert!(crate::db::start(), "数据库初始化失败");

    // 命令分发器（build 内部会注册全部服务）
    let config = arona::config::onebot::OneBotConfig {
        self_id: 10000,
        nickname: "Arona".to_string(),
        send_image_as_file: false,
        connections: std::collections::BTreeMap::new(),
    };
    let dispatcher = crate::standalone::dispatcher::build(config);

    let sender = Arc::new(CaptureSender {
        sent: Mutex::new(Vec::new()),
    });
    let context = Arc::new(CommandContext {
        user_id: 10001,
        group_id: Some(900000001),
        text: "/十连".to_string(),
        sender_name: Some("测试老师".to_string()),
        is_admin: true,
        sender: sender.clone(),
    });

    let handled = dispatcher.dispatch(context).await;
    assert!(handled, "调度器未识别 /十连");

    let messages: Vec<OutgoingMessage> = sender
        .sent
        .lock()
        .unwrap()
        .iter()
        .map(|(_, message)| message.clone())
        .collect();
    for message in &messages {
        println!("回复段: {:?}", message.segments);
        println!("撤回(毫秒): {:?}", message.revoke_after_millis);
    }

    let image_path = messages
        .iter()
        .flat_map(|message| message.segments.iter())
        .find_map(|segment| match segment {
            MessageSegment::Image {
                file: Some(file), ..
            } => Some(file.clone()),
            _ => None,
        });
    let Some(image_path) = image_path else {
        panic!("未生成抽卡结果图（可能网络失败回退成文本，见上方回复段）");
    };

    let image_file = std::path::PathBuf::from(&image_path);
    println!("抽卡结果图: {}", image_file.display());
    assert!(image_file.exists(), "结果图文件不存在: {image_path}");

    let expected_dir = image_root.join("gacha").join("result");
    let parent = image_file.parent().expect("结果图没有父目录");
    assert_eq!(
        parent.canonicalize().ok(),
        expected_dir.canonicalize().ok(),
        "结果图未落在 data/bluearchive/image/gacha/result: {}",
        image_file.display()
    );

    let decoded = image::open(&image_file).expect("读取抽卡结果图失败");
    let rgba = decoded.to_rgba8();
    assert_eq!(
        (rgba.width(), rgba.height()),
        (2340, 1080),
        "结果图尺寸异常"
    );
    println!("结果图尺寸: {}x{}", rgba.width(), rgba.height());

    // 生成缩略预览，便于人工确认
    let preview_path = image_file.with_extension("preview.png");
    let preview = decoded.resize(
        880,
        880 * decoded.height() / decoded.width(),
        image::imageops::FilterType::Triangle,
    );
    preview.save(&preview_path).expect("保存预览失败");
    println!("预览图: {}", preview_path.display());
}
