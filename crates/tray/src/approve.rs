//! Approving another device that is joining yours (ADR 0015), through this daemon's API: it must
//! hold your user key. The calls run on a thread of their own, so the UI never waits on them.

use std::path::Path;
use std::sync::mpsc;

use shoal_sdk::Client;

/// What approving shows, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approving {
    /// The device answered: approve it only if it shows `confirmation` too.
    Confirm {
        /// The name it suggests for itself.
        name: String,
        /// Its device ID.
        device: String,
        /// The confirmation code both show.
        confirmation: String,
    },
    /// Approved: it is one of your devices now.
    Approved,
    /// Declined, as asked.
    Declined,
    /// Not approved, and why.
    Failed(String),
}

/// A decision on the device asking.
enum Decision {
    Approve,
    Decline,
}

/// Approving the device whose pairing code was entered. Dropping it before deciding declines.
pub struct Approval {
    decisions: mpsc::Sender<Decision>,
}

impl Approval {
    /// Starts approving the device with pairing `code`, through the daemon of `data_dir`;
    /// `events` is given what to show, from a thread of its own.
    pub fn start(data_dir: &Path, code: &str, events: impl Fn(Approving) + Send + 'static) -> Self {
        let client = Client::local(Client::socket_path(data_dir));
        let code = code.trim().to_owned();
        let (decisions, decided) = mpsc::channel();
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => return events(Approving::Failed(e.to_string())),
            };
            let pairing = match runtime.block_on(client.pair(&code)) {
                Ok(pairing) => pairing,
                Err(e) => return events(Approving::Failed(e.to_string())),
            };
            events(Approving::Confirm {
                name: pairing.suggested_name.clone(),
                device: pairing.device.clone(),
                confirmation: pairing.confirmation_code.clone(),
            });
            let outcome = match decided.recv() {
                Ok(Decision::Approve) => runtime
                    .block_on(client.approve_pairing(&pairing.id, None))
                    .map(|()| Approving::Approved),
                Ok(Decision::Decline) | Err(_) => runtime
                    .block_on(client.decline_pairing(&pairing.id))
                    .map(|()| Approving::Declined),
            };
            events(outcome.unwrap_or_else(|e| Approving::Failed(e.to_string())));
        });
        Self { decisions }
    }

    /// Approves the device: it showed the same confirmation code.
    pub fn approve(&self) {
        let _ = self.decisions.send(Decision::Approve);
    }

    /// Declines it.
    pub fn decline(&self) {
        let _ = self.decisions.send(Decision::Decline);
    }
}
