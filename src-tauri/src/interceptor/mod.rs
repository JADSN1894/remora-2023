use anyhow::anyhow;
use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::fetch::{
    self, ContinueRequestParams, EventRequestPaused, FailRequestParams, FulfillRequestParams,
};
use chromiumoxide::cdp::browser_protocol::network::{
    self, ErrorReason, EventRequestWillBeSent, EventResponseReceived, ResourceType,
};
use chromiumoxide::Page;
use futures::{select, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::storage::RemoraStorage;

const CONTENT: &str = "<html><head><meta http-equiv=\"refresh\" content=\"0;URL='http://www.example.com/'\" /></head><body><h1>TEST</h1></body></html>";
const TARGET: &str = "http://google.com/";

struct SessionInfo {
    name: String,
    path: String,
}
pub struct RemoraInterceptor {
    state: Arc<Mutex<u64>>,
    session_name: String,
    storage: RemoraStorage,
}

pub struct RemoraInterceptorBuilderWithS {
    state: Arc<Mutex<u64>>,
}
impl RemoraInterceptorBuilderWithS {
    pub fn session_name<T: AsRef<str>>(self, session_name: T) -> RemoraInterceptorBuilderWithSS {
        let Self { state } = self;
        RemoraInterceptorBuilderWithSS {
            state,
            session_name: session_name.as_ref().to_string(),
        }
    }
}
pub struct RemoraInterceptorBuilderWithSS {
    state: Arc<Mutex<u64>>,
    session_name: String,
}

impl RemoraInterceptorBuilderWithSS {
    pub fn storage(self, storage: RemoraStorage) -> RemoraInterceptorBuilderWithSSD {
        let Self {
            state,
            session_name,
        } = self;

        RemoraInterceptorBuilderWithSSD {
            state,
            session_name,
            storage,
        }
    }
}

pub struct RemoraInterceptorBuilderWithSSD {
    state: Arc<Mutex<u64>>,
    session_name: String,
    storage: RemoraStorage,
}

impl RemoraInterceptorBuilderWithSSD {
    pub fn build(self) -> RemoraInterceptor {
        let Self {
            state,
            session_name,
            storage,
        } = self;
        RemoraInterceptor {
            state,
            session_name,
            storage,
        }
    }
}
impl RemoraInterceptor {
    pub fn new() -> RemoraInterceptorBuilderWithS {
        RemoraInterceptorBuilderWithS {
            state: Arc::new(Mutex::new(0)),
        }
    }

    pub async fn launch(self) -> anyhow::Result<()> {
        use url::Url;
        let session_info = SessionInfo {
            name: self.session_name().to_string(),
            path: Url::parse(self.storage().uri())?.path().to_string(),
        };

        Self::save_session_info(self.storage(), session_info).await?;

        launch_inteceptor(self).await
    }

    async fn save_session_info(
        remora_storage: &RemoraStorage,
        session_info: SessionInfo,
    ) -> anyhow::Result<()> {
        use crate::entities::{prelude::*, *};
        use sea_orm::*;
        let SessionInfo { name, path } = session_info;
        let session = session::ActiveModel {
            name: ActiveValue::Set(name),
            path: ActiveValue::Set(path),
            ..Default::default()
        };
        let res = Session::insert(session)
            .exec(remora_storage.connection())
            .await?;

        dbg!(&res);

        Ok(())
    }

    pub fn session_name(&self) -> &str {
        self.session_name.as_ref()
    }

    pub fn storage(&self) -> &RemoraStorage {
        &self.storage
    }
}

async fn launch_inteceptor(ctx: RemoraInterceptor) -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let (browser, mut handler) = Browser::launch(
        BrowserConfig::builder()
            .with_head()
            .viewport(None)
            .build()
            .map_err(|err| anyhow!(err))?,
    )
    .await?;

    let handle = tokio::task::spawn(async move {
        while let Some(h) = handler.next().await {
            if h.is_err() {
                break;
            }
        }
    });

    {
        // Setup request interception
        let page = Arc::new(browser.new_page("about:blank").await?);

        let mut request_will_be_sent = page
            .event_listener::<EventRequestWillBeSent>()
            .await
            .unwrap()
            .fuse();
        let mut request_paused = page
            .event_listener::<EventRequestPaused>()
            .await
            .unwrap()
            .fuse();
        let mut response_received = page
            .event_listener::<EventResponseReceived>()
            .await
            .unwrap()
            .fuse();

        let intercept_page = page.clone();
        let event_counter_arc_mutex: Arc<Mutex<u64>> = ctx.state.clone();

        let _ = tokio::task::spawn(async move {
            let mut resolutions: HashMap<network::RequestId, InterceptResolution> = HashMap::new();
            loop {
                select! {
                  event = response_received.next() => {
                        if let Some(event) = event {
                            let mut event_counter_mutex = event_counter_arc_mutex.lock().await;
                            *event_counter_mutex += 1;
                            // Responses
                            // dbg!(event);
                            const MAX_STRING_SIZE: usize = 100;

                            let sliced_url: String = match event.response.url.chars().nth(MAX_STRING_SIZE){
                                Some(_) => {
                                    let mut inner_sliced_url: String = event.response.url[..MAX_STRING_SIZE-4].to_string();
                                    inner_sliced_url.push_str(" ...");
                                    inner_sliced_url
                                },
                                None => event.response.url.to_string()
                            };


                            // TODO: Save event into database
                            println!("{event_counter_mutex:0000}: {sliced_url}");
                        }
                  },
                  event = request_paused.next() => {
                    if let Some(event) = event {
                        // Responses
                        if event.response_status_code.is_some() {
                            forward(&intercept_page, &event.request_id).await;
                            continue;
                        }

                        if let Some(network_id) = event.network_id.as_ref().map(|id| id.as_network_id()) {
                            let resolution = resolutions.entry(network_id.clone()).or_insert(InterceptResolution::new());
                            resolution.request_id = Some(event.request_id.clone());
                            if event.request.url == TARGET {
                              resolution.action = InterceptAction::Fullfill;
                            }
                            println!("paused: {resolution:?}, network: {network_id:?}");
                            resolve(&intercept_page, &network_id, &mut resolutions).await;
                        }
                      }
                  },
                  event = request_will_be_sent.next() => {


                      if let Some(event) = event {
                        // let mut event_counter_mutex = event_counter_arc_mutex.lock().await;
                        // *event_counter_mutex += 1;

                          let resolution = resolutions.entry(event.request_id.clone()).or_insert(InterceptResolution::new());
                          let action = if is_navigation(&event) {
                              InterceptAction::Abort
                          } else {
                              InterceptAction::Forward
                          };
                          resolution.action = action;
                        //   println!("sent: {resolution:?}");
                        //   println!("{event_counter_mutex}: sent: {resolution:?}");

                          resolve(&intercept_page, &event.request_id, &mut resolutions).await;
                      }
                  },
                  complete => break,
                }
            }

            println!("done");
        });
    }

    handle.await?;
    Ok(())
}

#[derive(Debug)]
enum InterceptAction {
    Forward,
    Abort,
    Fullfill,
    None,
}

#[derive(Debug)]
struct InterceptResolution {
    action: InterceptAction,
    request_id: Option<fetch::RequestId>,
}

impl InterceptResolution {
    pub fn new() -> Self {
        Self {
            action: InterceptAction::None,
            request_id: None,
        }
    }
}

trait RequestIdExt {
    fn as_network_id(&self) -> network::RequestId;
}

impl RequestIdExt for fetch::RequestId {
    fn as_network_id(&self) -> network::RequestId {
        network::RequestId::new(self.inner().clone())
    }
}

fn is_navigation(event: &EventRequestWillBeSent) -> bool {
    if event.request_id.inner() == event.loader_id.inner()
        && event
            .r#type
            .as_ref()
            .map(|t| *t == ResourceType::Document)
            .unwrap_or(false)
    {
        return true;
    }
    false
}

async fn resolve(
    page: &Page,
    network_id: &network::RequestId,
    resolutions: &mut HashMap<network::RequestId, InterceptResolution>,
) {
    if let Some(resolution) = resolutions.get(network_id) {
        if let Some(request_id) = &resolution.request_id {
            match resolution.action {
                InterceptAction::Forward => {
                    forward(page, request_id).await;
                    resolutions.remove(network_id);
                }
                InterceptAction::Abort => {
                    abort(page, request_id).await;
                    resolutions.remove(network_id);
                }
                InterceptAction::Fullfill => {
                    fullfill(page, request_id).await;
                    resolutions.remove(network_id);
                }
                InterceptAction::None => (), // Processed pausd but not will be sent
            }
        }
    }
}

async fn forward(page: &Page, request_id: &fetch::RequestId) {
    println!("Request {request_id:?} forwarded");
    if let Err(e) = page
        .execute(ContinueRequestParams::new(request_id.clone()))
        .await
    {
        println!("Failed to forward request: {e}");
    }
}

async fn abort(page: &Page, request_id: &fetch::RequestId) {
    println!("Request {request_id:?} aborted");
    if let Err(e) = page
        .execute(FailRequestParams::new(
            request_id.clone(),
            ErrorReason::Aborted,
        ))
        .await
    {
        println!("Failed to abort request: {e}");
    }
}

async fn fullfill(page: &Page, request_id: &fetch::RequestId) {
    println!("Request {request_id:?} fullfilled");
    if let Err(e) = page
        .execute(
            FulfillRequestParams::builder()
                .request_id(request_id.clone())
                .body(BASE64_STANDARD.encode(CONTENT))
                .response_code(200)
                .build()
                .unwrap(),
        )
        .await
    {
        println!("Failed to fullfill request: {e}");
    }
}
