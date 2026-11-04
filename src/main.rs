use fedengine_app::FedEngine;
use fedengine_core::Config;
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "fedengine=info".into()),
        )
        .init();
    let mut args = std::env::args().skip(1);
    let mut config = "config/application.toml".to_string();
    let mut check = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config = args.next().ok_or("--config requires a path")?,
            "--check-config" => check = true,
            "--help" | "-h" => {
                println!("fedengine [--config PATH] [--check-config]");
                return Ok(());
            }
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    let config = Config::load(config)?;
    if check {
        println!("configuration valid");
        return Ok(());
    }
    let engine = FedEngine::start(config).await?;
    tracing::info!(address=?engine.address(),management_address=?engine.management_address(),"FedEngine Ready");
    let run: Result<(), Box<dyn std::error::Error + Send + Sync>> = tokio::select! {result=wait_for_signal()=>result.map_err(Into::into),result=engine.handle.wait()=>result.map_err(Into::into)};
    let shutdown = engine.shutdown_until_drained().await;
    tracing::info!(receipt=?shutdown,"checked shutdown");
    run?;
    shutdown?;
    Ok(())
}
async fn wait_for_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {r=tokio::signal::ctrl_c()=>r,_=terminate.recv()=>Ok(())}
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
