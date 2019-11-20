// Client part, that is, the part that runs in the local process.
//
// All the futures based code lives here.
//
use std::collections::HashMap;
use std::default::Default;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Once};

use futures::prelude::*;
use futures::channel::{mpsc, oneshot};
use futures::join;

use tokio::prelude::*;
use tokio::net::UnixStream;
use tokio::net::unix::split::WriteHalf as UnixWriteHalf;
use tokio::net::unix::split::ReadHalf as UnixReadHalf;

use crate::pam::{PamError, ERR_RECV_FROM_SERVER, ERR_SEND_TO_SERVER};
use crate::pamserver::{PamResponse, PamServer};

// Request to be sent to the server process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PamRequest {
    pub id:      u64,
    pub user:    String,
    pub pass:    String,
    pub service: String,
    pub remip:   Option<String>,
}

// sent over request channel to PamAuthTask.
struct PamRequest1 {
    req:       PamRequest,
    resp_chan: oneshot::Sender<Result<(), PamError>>,
}

/// Pam authenticator.
#[derive(Clone)]
pub struct PamAuth {
    req_chan:   mpsc::Sender<PamRequest1>,
    task_once:  Arc<PamAuthTaskOnce>,
}

struct PamAuthTaskOnce {
    once:   Once,
    task:   Mutex<Option<PamAuthTask>>,
}

type PamAuthTask = Pin<Box<dyn Future<Output = io::Result<()>> + Send + 'static>>;

impl PamAuth {
    /// Create a new PAM authenticator. This will start a new PAM server process
    /// in the background, and it will contain a new PAM coordination task that
    /// will be lazily spawned the first time auth() is called.
    ///
    /// Note that it is important to call this very early in main(), before any
    /// threads or runtimes have started.
    ///
    /// ```no_run
    /// use pam_sandboxed::PamAuth;
    ///
    /// fn main() -> Result<(), Box<std::error::Error>> {
    ///     // get pam authentication handle.
    ///     let mut pam = PamAuth::new(None)?;
    ///
    ///     // now start tokio runtime and use handle.
    ///     let mut rt = tokio::runtime::Runtime::new()?;
    ///     rt.block_on(async move {
    ///         let res = pam.auth("other", "user", "pass", None).await;
    ///         println!("pam auth result: {:?}", res);
    ///     });
    ///     Ok(())
    /// }
    /// ```
    ///
    pub fn new(num_threads: Option<usize>) -> Result<PamAuth, io::Error> {
        let (req_chan, task) = PamAuthTaskBg::start(num_threads)?;
        Ok(PamAuth {
            req_chan,
            task_once: Arc::new(PamAuthTaskOnce{
                once:   Once::new(),
                task:   Mutex::new(Some(task)),
            }),
        })
    }

    /// Authenticate via pam and return the result.
    ///
    /// - `service`: PAM service to use - usually "other".
    /// - `username`: account username
    /// - `password`: account password
    /// - `remoteip`: if this is a networking service, the remote IP address of the client.
    pub async fn auth(
        &mut self,
        service: &str,
        username: &str,
        password: &str,
        remoteip: Option<&str>,
    ) -> Result<(), PamError>
    {
        // if we haven't started the background task yet, do it now.
        self.task_once.once.call_once(|| {
            let mut opt = self.task_once.task.lock().unwrap();
            let task = opt.take().unwrap();
            debug!("PamAuthTask: spawning task on runtime");
            tokio::spawn(async move {
                match task.await {
                    Ok(_) => debug!("PamAuthTask is done."),
                    Err(_e) => debug!("PamAuthTask future returned error: {}", _e),
                }
            });
        });

        // create request to be sent to the server.
        let req = PamRequest {
            id:      0,
            user:    username.to_string(),
            pass:    password.to_string(),
            service: service.to_string(),
            remip:   remoteip.map(|s| s.to_string()),
        };

        // add a one-shot channel for the response.
        let (tx, rx) = oneshot::channel::<Result<(), PamError>>();

        // put it all together and send it.
        let req1 = PamRequest1 {
            req:       req,
            resp_chan: tx,
        };
        self.req_chan.clone().send(req1).await.map_err(|_| PamError(ERR_SEND_TO_SERVER))?;

        // wait for the response.
        match rx.await {
            Ok(res) => res,
            Err(_) => Err(PamError(ERR_RECV_FROM_SERVER)),
        }
    }
}

// Shared data for the PamAuthTaskBg tasks.
struct PamAuthTaskBg {
    // clients waiting for a response.
    waiters: Mutex<HashMap<u64, oneshot::Sender<Result<(), PamError>>>>,
}

impl PamAuthTaskBg {

    // Start the server process. Then return a handle to send requests on.
    fn start(num_threads: Option<usize>) -> io::Result<(mpsc::Sender<PamRequest1>, PamAuthTask)> {
        // spawn the server process.
        let serversock = PamServer::start(num_threads)?;

        // transform standard unixstream to tokio version.
        let handle = tokio_net::driver::Handle::default();
        let mut serversock = UnixStream::from_std(serversock, &handle)?;

        // create a request channel.
        let (req_tx, req_rx) = mpsc::channel::<PamRequest1>(0);

        // shared state between request and response task.
        let this = PamAuthTaskBg{
            waiters: Mutex::new(HashMap::new()),
        };

        let task = Box::pin(async move {
            // split serversock into send/receive halves.
            let (srx, stx) = serversock.split();

            join!(this.handle_request(req_rx, stx), this.handle_response(srx));
            Ok(())
        });

        Ok((req_tx, task))
    }

    async fn handle_request(&self, mut req_rx: mpsc::Receiver<PamRequest1>, mut stx: UnixWriteHalf<'_>) {
        let mut id: u64 = 0;
        loop {
            // receive next request.
            let PamRequest1 { mut req, resp_chan } = match req_rx.next().await {
                Some(r1) => r1,
                None => {
                    // PamAuth handle was dropped. Ask server to exit.
                    let data = [0u8; 2];
                    let _ = stx.write_all(&data).await;
                    return;
                },
            };

            // store the response channel.
            req.id = id;
            id += 1;
            {
                let mut waiters = self.waiters.lock().unwrap();
                waiters.insert(req.id, resp_chan);
            }

            // serialize data and send.
            let mut data: Vec<u8> = match bincode::serialize(&req) {
                Ok(data) => data,
                Err(e) => {
                    // this panic can never happen at runtime.
                    panic!("PamClient: serializing data: {:?}", e);
                },
            };
            if data.len() > 65533 {
                // this panic can never happen at runtime.
                panic!("PamClient: serialized data > 65533 bytes");
            }
            let l1 = ((data.len() >> 8) & 0xff) as u8;
            let l2 = (data.len() & 0xff) as u8;
            data.insert(0, l1);
            data.insert(1, l2);
            if let Err(e) = stx.write_all(&data).await {
                // this can happen if the server has gone away.
                // in which case, handle_response() will exit as well.
                error!("PamClient: FATAL: writing data to server: {:?}", e);
                return;
            }
        }
    }

    async fn handle_response(&self, mut srx: UnixReadHalf<'_>) {
        loop {
            // read size header.
            let mut buf = [0u8; 2];
            if let Err(_) = srx.read_exact(&mut buf).await {
                error!("PamClient: FATAL: short read, server gone away?!");
                return;
            }
            let sz = ((buf[0] as usize) << 8) + (buf[1] as usize);

            // read response data.
            let mut data = Vec::with_capacity(sz);
            data.resize(sz, 0u8);
            if let Err(_) = srx.read_exact(&mut data[..]).await {
                error!("PamClient: FATAL: short read, server gone away?!");
                return;
            }

            // deserialize.
            let resp: PamResponse = match bincode::deserialize(&data[..]) {
                Ok(req) => req,
                Err(_) => {
                    // this panic can never happen at runtime.
                    panic!("PamCLient: error deserializing response");
                },
            };

            // and send response to waiting requester.
            let resp_chan = {
                let mut waiters = self.waiters.lock().unwrap();
                waiters.remove(&resp.id)
            };
            if let Some(resp_chan) = resp_chan {
                let _ = resp_chan.send(resp.result);
            }
        }
    }
}
