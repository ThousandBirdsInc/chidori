mod executor;
mod integrations;
mod runtime_nodes;
pub mod tonic_runtime;
mod db_operations;

#[cfg(feature = "nodejs")]
use neon::prelude::*;

use std::sync::mpsc;
use std::thread;

#[cfg(feature = "nodejs")]
use neon::{types::Deferred};

#[macro_use]
extern crate lazy_static;


#[cfg(feature = "nodejs")]
type DbCallback = Box<dyn FnOnce(&mut String, &Channel, Deferred) + Send>;


#[cfg(feature = "nodejs")]
struct ExecutorGRPCServer {
    tx: mpsc::Sender<GRPCServerMessage>,
}

// Messages sent on the server channel
#[cfg(feature = "nodejs")]
enum GRPCServerMessage {
    // Promise to resolve and callback to be executed
    // Deferred is threaded through the message instead of moved to the closure so that it
    // can be manually rejected.
    Callback(Deferred, DbCallback),
    // Indicates that the thread should be stopped and connection closed
    Close,
}

// Clean-up when Database is garbage collected, could go here
// but, it's not needed,
#[cfg(feature = "nodejs")]
impl Finalize for ExecutorGRPCServer {}

// Internal implementation
#[cfg(feature = "nodejs")]
impl ExecutorGRPCServer {
    fn new<'a, C>(port: String, mut cx: &mut C) -> Result<Self, String> where C: Context<'a>, {
        let (tx, rx) = mpsc::channel::<GRPCServerMessage>();
        let channel = cx.channel();
        thread::spawn(move || {
            tonic_runtime::run_server(port, None);
            while let Ok(message) = rx.recv() {
                match message {
                    GRPCServerMessage::Callback(deferred, f) => {
                    }
                    GRPCServerMessage::Close => break,
                }
            }
        });
        Ok(Self { tx })
    }

    // Idiomatic rust would take an owned `self` to prevent use after close
    // However, it's not possible to prevent JavaScript from continuing to hold a closed database
    fn close(&self) -> Result<(), mpsc::SendError<GRPCServerMessage>> {
        self.tx.send(GRPCServerMessage::Close)
    }

    fn send(
        &self,
        deferred: Deferred,
        callback: impl FnOnce(&mut String, &Channel, Deferred) + Send + 'static,
    ) -> Result<(), mpsc::SendError<GRPCServerMessage>> {
        self.tx
            .send(GRPCServerMessage::Callback(deferred, Box::new(callback)))
    }
}

#[cfg(feature = "nodejs")]
impl ExecutorGRPCServer {
    fn js_new(mut cx: FunctionContext) -> JsResult<JsBox<ExecutorGRPCServer>> {
        let port = cx.argument::<JsString>(0)?.value(&mut cx);
        let db = ExecutorGRPCServer::new(port, &mut cx).or_else(|err| cx.throw_error(err.to_string()))?;
        Ok(cx.boxed(db))
    }

    fn js_close(mut cx: FunctionContext) -> JsResult<JsUndefined> {
        cx.this()
            .downcast_or_throw::<JsBox<ExecutorGRPCServer>, _>(&mut cx)?
            .close()
            .or_else(|err| cx.throw_error(err.to_string()))?;
        Ok(cx.undefined())
    }

}

#[cfg(feature = "nodejs")]
trait SendResultExt {
    fn into_rejection<'a, C: Context<'a>>(self, cx: &mut C) -> NeonResult<()>;
}

#[cfg(feature = "nodejs")]
impl SendResultExt for Result<(), mpsc::SendError<GRPCServerMessage>> {
    fn into_rejection<'a, C: Context<'a>>(self, cx: &mut C) -> NeonResult<()> {
        self.or_else(|err| {
            let msg = err.to_string();
            match err.0 {
                GRPCServerMessage::Callback(deferred, _) => {
                    let err = cx.error(msg)?;
                    deferred.reject(cx, err);
                    Ok(())
                }
                GRPCServerMessage::Close => cx.throw_error("Expected DbMessage::Callback"),
            }
        })
    }
}

#[cfg(feature = "nodejs")]
fn neon_start_server(mut cx: FunctionContext) -> JsResult<JsNumber> {
    let port = cx.argument::<JsString>(0)?.value(&mut cx);
    let file_path: Option<JsResult<JsString>> = match cx.argument_opt(0) {
        Some(v) => Some(v.downcast_or_throw(&mut cx)),
        None => None,
    };
    let file_path_v = file_path.map(|p| p.unwrap().value(&mut cx));
    std::thread::spawn(|| {
        tonic_runtime::run_server(port, file_path_v);
    });
    Ok(cx.number(1.0 as f64))
}

#[cfg(feature = "nodejs")]
#[neon::main]
fn neon_main(mut cx: ModuleContext) -> NeonResult<()> {
    cx.export_function("serverNew", ExecutorGRPCServer::js_new)?;
    cx.export_function("serverClose", ExecutorGRPCServer::js_close)?;
    cx.export_function("startServer", neon_start_server)?;
    Ok(())
}
