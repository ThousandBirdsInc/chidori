use std::collections::HashMap;
use std::time::Duration;
use fantoccini::{ClientBuilder, Locator};
use serde_json::json;
use crate::execution::primitives::operation::AsyncRPCCommunication;
use crate::execution::primitives::serialized_value::RkyvSerializedValue;

async fn create_interactive_browser_session(mut async_rpccommunication: AsyncRPCCommunication) -> Result<(), fantoccini::error::CmdError>  {
    let c = ClientBuilder::native().connect("http://localhost:4444").await.expect("failed to connect to WebDriver");
    async_rpccommunication.callable_interface_sender.send(vec!["run".to_string()]).unwrap();
    tokio::spawn(async move {
        loop {
            if let Ok((key, value, sender)) = async_rpccommunication.receiver.try_recv() {
                match key.as_str() {
                    "run" => {
                        // first, go to the Wikipedia page for Foobar
                        c.goto("https://en.wikipedia.org/wiki/Foobar").await?;
                        let url = c.current_url().await?;
                        assert_eq!(url.as_ref(), "https://en.wikipedia.org/wiki/Foobar");

                        // click "Foo (disambiguation)"
                        c.find(Locator::Css(".mw-disambig")).await?.click().await?;

                        // click "Foo Lake"
                        c.find(Locator::LinkText("Foo Lake")).await?.click().await?;

                        let url = c.current_url().await?;
                        assert_eq!(url.as_ref(), "https://en.wikipedia.org/wiki/Foo_Lake");

                        sender.send(RkyvSerializedValue::String(format!("{}", 1))).unwrap();
                    }
                    _ => {}
                }
            } else {
                tokio::time::sleep(Duration::from_millis(10)).await; // Sleep for 10 milliseconds
            }
        }
        anyhow::Ok::<()>(())
    }).await.unwrap();
    Ok(())
}

