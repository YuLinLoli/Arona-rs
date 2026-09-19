//! 「收到消息立刻打印」端到端自检：起一个假 OneBot 端（真实 WebSocket），
//! 应用用 ws-forward 连上去，假端推一条群消息事件且**故意不回复 get_group_info**，
//! 断言控制台日志在 2 秒内就出现「收到 xxx -> ...」。
//!
//! 旧实现会把这条日志排队，等 get_group_info 返回（这里永远不返回，最长等 5 秒
//! 超时）后才补打；同时命令是并行走的，于是出现「抽卡结果先刷出来、『收到指令』
//! 后到」的错位，看起来就是控制台卡住了。本用例锁死「先打印、再处理」这个顺序。
//!
//! 全程本机回环（127.0.0.1），不需要外网。运行:
//!   cargo test onebot_print -- --nocapture --test-threads=1

use arona::config::onebot::OneBotConfig;
use arona::onebot::application::OneBotApplication;
use arona::onebot::business::StandaloneBusinessHandler;
use arona::onebot::connection::ConnectionRegistry;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MARKER: &str = "端到端时序标记-0xdeadbeef";

#[tokio::test]
async fn group_message_is_logged_immediately_over_ws_forward() {
    arona::runtime::config::set_bot_id(10000);
    // 独立的日志文件/数据目录，避免污染仓库
    arona::runtime::services::set_data_root(arona::runtime::paths::data_root());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("无法绑定回环端口");
    let port = listener.local_addr().unwrap().port();

    // 假 OneBot 端
    let (sent_tx, sent_rx) = tokio::sync::oneshot::channel::<Instant>();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        // 等应用侧把连接注册好再推事件
        tokio::time::sleep(Duration::from_millis(300)).await;
        let event = json!({
            "post_type": "message",
            "message_type": "group",
            "sub_type": "normal",
            "time": 0,
            "self_id": 10000,
            "message_id": 1,
            "user_id": 10001,
            "group_id": 900000123,
            "raw_message": MARKER,
            "message": [{ "type": "text", "data": { "text": MARKER } }],
            "sender": { "nickname": "测试老师" }
        });
        let _ = ws
            .send(tokio_tungstenite::tungstenite::Message::Text(
                event.to_string(),
            ))
            .await;
        let _ = sent_tx.send(Instant::now());
        // 之后既不回 get_group_info 也不回 get_status，只是把连接挂住
        let mut sink_open = ws.next().await;
        while let Some(Ok(_)) = sink_open {
            sink_open = ws.next().await;
        }
    });

    // 应用侧：只启用 ws-forward，指向假端
    let mut config = OneBotConfig::default();
    for conn in config.connections.values_mut() {
        conn.enable = false;
    }
    {
        let conn = config
            .connections
            .get_mut("ws-forward")
            .expect("默认配置应包含 ws-forward");
        conn.connection_type = "ws-forward".to_string();
        conn.enable = true;
        conn.url = format!("ws://127.0.0.1:{port}");
        conn.token = String::new();
    }
    let registry = Arc::new(ConnectionRegistry::new());
    let dispatcher = crate::standalone::dispatcher::build(config.clone());
    let business = Arc::new(StandaloneBusinessHandler::new(
        config.clone(),
        dispatcher,
        registry.clone(),
    ));
    let app = Arc::new(OneBotApplication::new(config.clone(), business, registry));
    app.start();

    let sent_at = tokio::time::timeout(Duration::from_secs(10), sent_rx)
        .await
        .expect("应用侧 10 秒内没连上假 OneBot 端")
        .expect("假端发送事件失败");

    let mut elapsed = None;
    for _ in 0..200 {
        if arona::runtime::log::live_lines()
            .iter()
            .any(|line| line.text.contains(MARKER))
        {
            elapsed = Some(sent_at.elapsed());
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    app.stop();
    server.abort();

    let elapsed = elapsed.expect("10 秒内都没有打印出收到的消息");
    assert!(
        elapsed < Duration::from_secs(2),
        "收到群消息必须在 2 秒内打印出来（实测 {elapsed:?}）：说明这条日志又被 get_group_info 挡住了"
    );

    // 顺带确认这一行确实带上了群号（群名未解析时的降级显示），命令结果不会跑到它前面
    let printed = arona::runtime::log::live_lines()
        .into_iter()
        .find(|line| line.text.contains(MARKER))
        .map(|line| line.text)
        .unwrap_or_default();
    assert!(
        printed.contains("900000123"),
        "群名未解析时应按群号显示，实际: {printed}"
    );
    assert!(
        printed.contains("测试老师"),
        "应显示发送者昵称，实际: {printed}"
    );
}
