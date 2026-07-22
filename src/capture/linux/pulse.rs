use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use libpulse_binding as pulse;
use pulse::callbacks::ListResult;
use pulse::context::{Context, FlagSet as ContextFlagSet, State as ContextState};
use pulse::mainloop::threaded::Mainloop;
use pulse::proplist::Proplist;
use pulse::sample::Spec;

const OP_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub(crate) struct Sink {
    pub index: u32,
    pub name: String,
    pub description: String,
    pub monitor_source_name: Option<String>,
    pub sample_spec: Spec,
}

#[derive(Debug, Clone)]
pub(crate) struct Source {
    pub name: String,
    pub description: String,
    pub monitor_of_sink: Option<u32>,
    pub sample_spec: Spec,
}

#[derive(Debug, Clone)]
pub(crate) struct SinkInput {
    pub index: u32,
    pub sink: u32,
    pub sample_spec: Spec,
    pub corked: bool,
    pub pid: Option<u32>,
    pub application_name: Option<String>,
    pub process_binary: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Server {
    pub default_sink_name: Option<String>,
    pub default_source_name: Option<String>,
}

pub(crate) struct PulseClient {
    mainloop: Rc<RefCell<Mainloop>>,
    context: Rc<RefCell<Context>>,
}

impl PulseClient {
    pub fn connect(application_name: &str) -> Result<Self, String> {
        let mainloop = Rc::new(RefCell::new(
            Mainloop::new().ok_or_else(|| "pa_threaded_mainloop_new failed".to_string())?,
        ));
        let mut proplist = Proplist::new().ok_or_else(|| "pa_proplist_new failed".to_string())?;
        proplist
            .set_str(
                pulse::proplist::properties::APPLICATION_NAME,
                application_name,
            )
            .map_err(|()| "cannot set PulseAudio application.name".to_string())?;
        let context = Context::new_with_proplist(&*mainloop.borrow(), application_name, &proplist)
            .ok_or_else(|| "pa_context_new_with_proplist failed".to_string())?;
        let context = Rc::new(RefCell::new(context));

        context
            .borrow_mut()
            .connect(None, ContextFlagSet::NOFLAGS, None)
            .map_err(|e| format!("pa_context_connect: {e}"))?;
        mainloop.borrow_mut().lock();
        if let Err(e) = mainloop.borrow_mut().start() {
            mainloop.borrow_mut().unlock();
            return Err(format!("pa_threaded_mainloop_start: {e}"));
        }
        mainloop.borrow_mut().unlock();

        let client = Self { mainloop, context };
        let deadline = Instant::now() + OP_TIMEOUT;
        loop {
            let state = client.context_state();
            match state {
                ContextState::Ready => return Ok(client),
                ContextState::Failed | ContextState::Terminated => {
                    let errno = client.with_context(|ctx| ctx.errno());
                    return Err(format!("PulseAudio context {state:?}: {errno}"));
                }
                _ if Instant::now() >= deadline => {
                    return Err("timed out connecting to PulseAudio".into());
                }
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    pub fn mainloop(&self) -> Rc<RefCell<Mainloop>> {
        Rc::clone(&self.mainloop)
    }

    pub fn context(&self) -> Rc<RefCell<Context>> {
        Rc::clone(&self.context)
    }

    pub fn lock(&self) {
        self.mainloop.borrow_mut().lock();
    }

    pub fn unlock(&self) {
        self.mainloop.borrow_mut().unlock();
    }

    pub fn context_state(&self) -> ContextState {
        self.with_context(|ctx| ctx.get_state())
    }

    pub fn with_context<T>(&self, f: impl FnOnce(&mut Context) -> T) -> T {
        self.lock();
        let value = f(&mut self.context.borrow_mut());
        self.unlock();
        value
    }

    pub fn list_sinks(&self) -> Result<Vec<Sink>, String> {
        let (tx, rx) = mpsc::channel();
        self.lock();
        let operation = self
            .context
            .borrow()
            .introspect()
            .get_sink_info_list(move |result| {
                let item = match result {
                    ListResult::Item(info) => Some(Ok(Some(Sink {
                        index: info.index,
                        name: info.name.as_deref().unwrap_or_default().to_string(),
                        description: info.description.as_deref().unwrap_or_default().to_string(),
                        monitor_source_name: info
                            .monitor_source_name
                            .as_deref()
                            .map(str::to_string),
                        sample_spec: info.sample_spec,
                    }))),
                    ListResult::End => Some(Ok(None)),
                    ListResult::Error => Some(Err("PulseAudio sink introspection failed".into())),
                };
                if let Some(item) = item {
                    let _ = tx.send(item);
                }
            });
        self.unlock();
        collect_list(rx, operation)
    }

    pub fn list_sources(&self) -> Result<Vec<Source>, String> {
        let (tx, rx) = mpsc::channel();
        self.lock();
        let operation = self
            .context
            .borrow()
            .introspect()
            .get_source_info_list(move |result| {
                let item = match result {
                    ListResult::Item(info) => Some(Ok(Some(Source {
                        name: info.name.as_deref().unwrap_or_default().to_string(),
                        description: info.description.as_deref().unwrap_or_default().to_string(),
                        monitor_of_sink: info.monitor_of_sink,
                        sample_spec: info.sample_spec,
                    }))),
                    ListResult::End => Some(Ok(None)),
                    ListResult::Error => Some(Err("PulseAudio source introspection failed".into())),
                };
                if let Some(item) = item {
                    let _ = tx.send(item);
                }
            });
        self.unlock();
        collect_list(rx, operation)
    }

    pub fn list_sink_inputs(&self) -> Result<Vec<SinkInput>, String> {
        let (tx, rx) = mpsc::channel();
        self.lock();
        let operation =
            self.context
                .borrow()
                .introspect()
                .get_sink_input_info_list(move |result| {
                    let item = match result {
                        ListResult::Item(info) => Some(Ok(Some(SinkInput {
                            index: info.index,
                            sink: info.sink,
                            sample_spec: info.sample_spec,
                            corked: info.corked,
                            pid: info
                                .proplist
                                .get_str(pulse::proplist::properties::APPLICATION_PROCESS_ID)
                                .and_then(|v| v.parse().ok()),
                            application_name: info
                                .proplist
                                .get_str(pulse::proplist::properties::APPLICATION_NAME),
                            process_binary: info
                                .proplist
                                .get_str(pulse::proplist::properties::APPLICATION_PROCESS_BINARY),
                        }))),
                        ListResult::End => Some(Ok(None)),
                        ListResult::Error => {
                            Some(Err("PulseAudio sink-input introspection failed".into()))
                        }
                    };
                    if let Some(item) = item {
                        let _ = tx.send(item);
                    }
                });
        self.unlock();
        collect_list(rx, operation)
    }

    pub fn server(&self) -> Result<Server, String> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.lock();
        let operation = self
            .context
            .borrow()
            .introspect()
            .get_server_info(move |info| {
                let _ = tx.send(Server {
                    default_sink_name: info.default_sink_name.as_deref().map(str::to_string),
                    default_source_name: info.default_source_name.as_deref().map(str::to_string),
                });
            });
        self.unlock();
        let result = rx
            .recv_timeout(OP_TIMEOUT)
            .map_err(|_| "timed out reading PulseAudio server info".to_string());
        drop(operation);
        result
    }
}

impl Drop for PulseClient {
    fn drop(&mut self) {
        self.lock();
        self.context.borrow_mut().disconnect();
        self.unlock();
        self.mainloop.borrow_mut().stop();
    }
}

fn collect_list<T, C>(
    rx: mpsc::Receiver<Result<Option<T>, String>>,
    operation: pulse::operation::Operation<C>,
) -> Result<Vec<T>, String>
where
    C: ?Sized,
{
    let deadline = Instant::now() + OP_TIMEOUT;
    let mut values = Vec::new();
    loop {
        let timeout = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(timeout) {
            Ok(Ok(Some(value))) => values.push(value),
            Ok(Ok(None)) => break,
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err("PulseAudio introspection timed out".into()),
        }
    }
    drop(operation);
    Ok(values)
}
