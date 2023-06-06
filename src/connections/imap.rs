use std::net::{Shutdown, TcpStream};

use async_trait::async_trait;
use imap::{
    types::{Fetch, UnsolicitedResponse},
    ClientBuilder, Session,
};
use mail_parser::{HeaderValue, Message as EmailParser};
use native_tls::{TlsConnector, TlsStream};
use tokio::sync::mpsc;

use crate::message::{Message, Receiver, Transport};

#[derive(Clone)]
pub struct Imap {
    pub domain: String,
    pub port: u16,
    pub user: String,
    pub password: String,
}

#[async_trait]
impl Transport for Imap {
    type Connection = ImapConnection;
    type Error = imap::Error;

    async fn connect(&self) -> imap::Result<ImapConnection> {
        let (session, tcp) = tokio::task::block_in_place(move || -> imap::Result<_> {
            let mut tcp_stream = None;
            let client = ClientBuilder::new(&self.domain, self.port).connect(|domain, tcp| {
                tcp_stream = Some(tcp.try_clone()?);
                let ssl_conn = TlsConnector::builder().build()?;
                Ok(TlsConnector::connect(&ssl_conn, domain, tcp)?)
            })?;

            let mut session = client
                .login(&self.user, &self.password)
                .map_err(|(e, _)| e)?;

            session.select("INBOX")?;
            Ok((session, tcp_stream.expect("an stream if connected")))
        })?;

        let (tx, rx) = mpsc::channel(1);

        tokio::task::spawn_blocking(move || {
            let err = listener(session, tx.clone()).expect_err("listener only ends at error");
            tx.blocking_send(Err(err)).ok();
        });

        Ok(ImapConnection { rx, tcp })
    }
}

fn listener(
    mut session: Session<TlsStream<TcpStream>>,
    tx: mpsc::Sender<imap::Result<Message>>,
) -> imap::Result<()> {
    loop {
        let fetches = session.fetch("1:*", "RFC822")?;

        for fetch in fetches.iter() {
            if let Ok(msg) = read_message(fetch) {
                tx.blocking_send(Ok(msg)).ok();

                // We want to be sure we only remove the message
                // if it has been read for processing
                session.store(fetch.message.to_string(), "+FLAGS (\\Deleted)")?;
                session.expunge()?;
            }
        }

        session.idle().wait_while(|response| match response {
            UnsolicitedResponse::Exists(_) => false,
            _ => true,
        })?;
    }
}

fn read_message(fetch: &Fetch<'_>) -> Result<Message, ()> {
    let email = EmailParser::parse(fetch.body().unwrap()).unwrap();

    let from = match email.from() {
        HeaderValue::Address(addr) => addr.address.clone().unwrap().into(),
        _ => return Err(()),
    };

    Ok(Message {
        address: from,
        subject: email.subject().unwrap_or("").into(),
        body: Vec::default(),
    })
}

pub struct ImapConnection {
    rx: mpsc::Receiver<imap::Result<Message>>,
    tcp: TcpStream,
}

#[async_trait]
impl Receiver for ImapConnection {
    type Error = imap::Error;

    async fn recv(&mut self) -> imap::Result<Message> {
        match self.rx.recv().await {
            Some(message) => message,
            None => unreachable!(),
        }
    }
}

impl Drop for ImapConnection {
    fn drop(&mut self) {
        self.tcp.shutdown(Shutdown::Both).ok();
    }
}
