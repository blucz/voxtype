//! Exercise Stop-time focus capture and delivery fallback with a fake hyprctl.
//! Run in an isolated XDG_RUNTIME_DIR, with FOCUS_TEST_FILE controlling focus.
use std::sync::{Arc, Mutex};
use voxtype::error::OutputError;
use voxtype::output::{self, OutputOptions, TextOutput};
struct Sink(&'static str, Arc<Mutex<Vec<String>>>);
#[async_trait::async_trait]
impl TextOutput for Sink {
    async fn output(&self, _: &str) -> Result<(), OutputError> {
        self.1.lock().unwrap().push(self.0.into());
        Ok(())
    }
    async fn is_available(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        self.0
    }
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let file = std::env::var("FOCUS_TEST_FILE")?;
    std::fs::create_dir_all(voxtype::config::Config::runtime_dir())?;
    let log = Arc::new(Mutex::new(Vec::new()));
    let chain: Vec<Box<dyn TextOutput>> = vec![
        Box::new(Sink("paste-test", log.clone())),
        Box::new(Sink("clipboard", log.clone())),
    ];
    let write =
        |address: &str| std::fs::write(&file, format!(r#"{{"address":"{address}","pid":123}}"#));
    write("0x1")?;
    output::focus::capture().await;
    assert!(output::focus::permits_delivery().await);
    write("0x2")?;
    output::output_with_fallback(
        &chain,
        "dictation",
        OutputOptions {
            pre_output_command: None,
            post_output_command: None,
            wait_for_modifier_release: false,
            modifier_release_timeout: std::time::Duration::ZERO,
        },
    )
    .await?;
    assert_eq!(*log.lock().unwrap(), vec!["clipboard"]);
    write("0x1")?;
    assert!(output::focus::permits_delivery().await);
    std::fs::write(&file, "{}")?;
    assert!(!output::focus::permits_delivery().await);
    output::focus::capture().await;
    assert!(!output::focus::permits_delivery().await);
    output::focus::clear();
    assert!(output::focus::permits_delivery().await);
    println!("PASS: unchanged focus allowed; switched/unknown windows withheld; clipboard fallback used; guard clears");
    Ok(())
}
