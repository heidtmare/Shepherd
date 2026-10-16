//! SMTP delivery of e-mail notices.

use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor, message::Mailbox,
    message::header::ContentType,
};
use uuid::Uuid;

#[derive(Clone)]
pub struct Mailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl Mailer {
    /// `url`: `smtp://host:port` (plain, or STARTTLS with `?tls=required`) or `smtps://user:pass@host`.
    pub fn new(url: &str, from: &str) -> anyhow::Result<Self> {
        Ok(Self {
            transport: AsyncSmtpTransport::<Tokio1Executor>::from_url(url)
                .map_err(|e| anyhow::anyhow!("SMTP_URL: {e}"))?
                .build(),
            from: from
                .parse()
                .map_err(|e| anyhow::anyhow!("EMAIL_FROM: {e}"))?,
        })
    }

    /// Sends a plain-text message; returns its Message-ID.
    pub async fn send(
        &self,
        delivery_id: Uuid,
        to: &str,
        subject: &str,
        body: &str,
    ) -> anyhow::Result<String> {
        let domain = self.from.email.domain().to_owned();
        let message_id = format!("<{delivery_id}@{domain}>");
        let msg = Message::builder()
            .from(self.from.clone())
            .to(to
                .parse()
                .map_err(|e| anyhow::anyhow!("recipient {to:?}: {e}"))?)
            .subject(subject)
            .message_id(Some(message_id.clone()))
            .header(ContentType::TEXT_PLAIN)
            .body(body.to_owned())?;
        self.transport.send(msg).await?;
        Ok(message_id)
    }
}
