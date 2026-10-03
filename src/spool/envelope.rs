//! What a spooled row keeps besides the body: the envelope and the session
//! facts the rewrite and the walk need again (D-116). Stored as JSON the
//! database never looks inside.
//!
//! The authenticated *username* is not kept — only that there was one, which is
//! all §6.1 step 8's `Received:` uses (D-071 keeps who-authenticated out of the
//! output). A spool row should hold no more than a delivery needs.

use serde::{Deserialize, Serialize};

use crate::relay::{OwnedEnvelope, OwnedMessage};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub mail_from: Option<String>,
    pub recipients: Vec<String>,
    pub smtputf8: bool,
    pub body_8bitmime: bool,
    pub helo: String,
    pub peer: std::net::IpAddr,
    pub authenticated: bool,
    pub tls: bool,
    pub correlation_id: String,
    /// The `From:` address §5.4 parsed at acceptance, so a retry resolves the
    /// sender rule exactly as acceptance did.
    pub from_header: Option<String>,
    /// §5.8's rule that chose the ramp, for the logs.
    pub ramp_source: String,
}

impl Envelope {
    pub fn of(message: &OwnedMessage, from_header: Option<&str>, ramp_source: &str) -> Self {
        Self {
            mail_from: message.envelope.mail_from.clone(),
            recipients: message.envelope.recipients.clone(),
            smtputf8: message.envelope.smtputf8,
            body_8bitmime: message.envelope.body_8bitmime,
            helo: message.helo.clone(),
            peer: message.peer,
            authenticated: message.auth.is_some(),
            tls: message.tls,
            correlation_id: message.correlation_id.clone(),
            from_header: from_header.map(str::to_string),
            ramp_source: ramp_source.to_string(),
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("an envelope always serialises")
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// The owned message the relay takes, with `body` and the instant it was
    /// received.
    pub fn into_message(
        self,
        body: Vec<u8>,
        received_at: chrono::DateTime<chrono::Utc>,
    ) -> OwnedMessage {
        OwnedMessage {
            envelope: OwnedEnvelope {
                mail_from: self.mail_from,
                recipients: self.recipients,
                smtputf8: self.smtputf8,
                body_8bitmime: self.body_8bitmime,
            },
            body,
            helo: self.helo,
            peer: self.peer,
            // Who is not kept; that someone did is (module docs).
            auth: self.authenticated.then(String::new),
            tls: self.tls,
            received_at,
            correlation_id: self.correlation_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_envelope_round_trips_and_forgets_the_username() {
        let message = OwnedMessage {
            envelope: OwnedEnvelope {
                mail_from: Some("a@oldbrand.com".into()),
                recipients: vec!["b@example.com".into()],
                smtputf8: true,
                body_8bitmime: false,
            },
            body: b"x".to_vec(),
            helo: "app".into(),
            peer: "10.0.0.1".parse().unwrap(),
            auth: Some("cfapp".into()),
            tls: true,
            received_at: chrono::DateTime::from_timestamp(1_767_225_600, 0).unwrap(),
            correlation_id: "cid".into(),
        };
        let env = Envelope::of(&message, Some("a@oldbrand.com"), "default");
        let json = env.to_json();
        assert!(!json.contains("cfapp"), "{json}");
        let back = Envelope::from_json(&json).unwrap();
        assert_eq!(back, env);
        let rebuilt = back.into_message(b"x".to_vec(), message.received_at);
        assert_eq!(rebuilt.envelope, message.envelope);
        assert!(rebuilt.auth.is_some());
        assert_eq!(rebuilt.peer, message.peer);
    }
}
