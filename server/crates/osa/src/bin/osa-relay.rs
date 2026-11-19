//! osa-relay：手机 App 的推送中转（单独运行，见 osa::relay）。和 `uvicorn osa.relay:app --host … --port …` 一样的用法。

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    osa::relay::main().await
}
