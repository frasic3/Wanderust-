use serde::{Deserialize, Serialize};

pub const MAX_USERNAME_LEN: usize = 32;
pub const MAX_PASSWORD_LEN: usize = 128;
pub const MAX_CHAT_LEN: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Message {
    #[serde(rename = "REGISTER")]
    Register { username: String, password: String },

    #[serde(rename = "LOGIN")]
    Login { username: String, password: String },

    #[serde(rename = "AUTH_OK")]
    AuthOk { token: String },

    #[serde(rename = "START_TRIP")]
    StartTrip {
        token: String,
        lat: f64,
        lon: f64,
        ts: i64,
    },

    #[serde(rename = "TRIP_STARTED")]
    TripStarted {
        trip_id: i64,
        lat: f64,
        lon: f64,
        ts: i64,
    },

    #[serde(rename = "POSITION")]
    Position {
        token: String,
        trip_id: i64,
        lat: f64,
        lon: f64,
        ts: i64,
    },

    #[serde(rename = "END_TRIP")]
    EndTrip {
        token: String,
        trip_id: i64,
        ts: i64,
    },

    #[serde(rename = "STATS")]
    Stats { //necessario per le statistiche 
        token: String,
        from_ts: i64, // timestamp inizio intervallo (epoch seconds)
        to_ts: i64, // timestamp fine intervallo (epoch seconds)
    },

    #[serde(rename = "STATS_RESULT")]
    StatsResult { //risultato delle statistiche 
        username: String,
        from_ts: i64,
        to_ts: i64,
        distance_m: f64,
        movement_secs: i64,
        pause_secs: i64,
        total_secs: i64,
        avg_speed_mps: f64,
        avg_speed_kmh: f64,
        points: i64,
    },

    #[serde(rename = "ACK")]
    Ack,

    #[serde(rename = "ERROR")]
    Error { code: String, message: String },

    // Messaggio di chat inviato dal client al server.
    // - `token`: identifica l'utente autenticato (ricevuto da AUTH_OK).
    // - `text`: testo del messaggio da inoltrare in chat.
    #[serde(rename = "CHAT")]
    ChatToServer { token: String, text: String },

    // Messaggio di chat che il server invia ai client.
    // - `from`: mittente. Se assente (None) il messaggio è un annuncio/broadcast
    //   di sistema; se presente è un messaggio inviato da un altro utente.
    //   `skip_serializing_if` evita di scrivere il campo nel JSON quando è None.
    // - `text`: testo del messaggio ricevuto.
    #[serde(rename = "MESSAGE")]
    ChatFromServer {
        #[serde(skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        text: String,
    },

    #[serde(rename = "DISCONNECT")]
    Disconnect { token: String },
}

pub fn encode(msg: &Message) -> serde_json::Result<String> {
    serde_json::to_string(msg)
}

pub fn decode(line: &str) -> serde_json::Result<Message> {
    serde_json::from_str(line)
}

pub fn validate_username(u: &str) -> Result<(), &'static str> {
    if u.is_empty() {
        return Err("username vuoto");
    }
    if u.len() > MAX_USERNAME_LEN {
        return Err("username troppo lungo");
    }
    if !u
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err("username: solo caratteri alfanumerici, '_' o '-'");
    }
    Ok(())
}

pub fn validate_password(p: &str) -> Result<(), &'static str> {
    if p.len() < 4 {
        return Err("password troppo corta (min 4)");
    }
    if p.len() > MAX_PASSWORD_LEN {
        return Err("password troppo lunga");
    }
    Ok(())
}

pub fn validate_chat(t: &str) -> Result<(), &'static str> {
    if t.is_empty() {
        return Err("messaggio vuoto");
    }
    if t.len() > MAX_CHAT_LEN {
        return Err("messaggio troppo lungo");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: Message) {
        let encoded = encode(&msg).expect("encode");
        let decoded = decode(&encoded).expect("decode");
        assert_eq!(msg, decoded, "roundtrip mismatch: {encoded}");
    }

    #[test]
    fn roundtrip_register() {
        roundtrip(Message::Register {
            username: "mario".into(),
            password: "hash".into(),
        });
    }

    #[test]
    fn roundtrip_login() {
        roundtrip(Message::Login {
            username: "mario".into(),
            password: "hash".into(),
        });
    }

    #[test]
    fn roundtrip_auth_ok() {
        roundtrip(Message::AuthOk {
            token: "abc123".into(),
        });
    }

    #[test]
    fn roundtrip_start_trip() {
        roundtrip(Message::StartTrip {
            token: "abc123".into(),
            lat: 45.0,
            lon: 9.0,
            ts: 1_700_000_000,
        });
    }

    #[test]
    fn roundtrip_trip_started() {
        roundtrip(Message::TripStarted {
            trip_id: 42,
            lat: 45.0,
            lon: 9.0,
            ts: 1_700_000_000,
        });
    }

    #[test]
    fn roundtrip_end_trip() {
        roundtrip(Message::EndTrip {
            token: "abc123".into(),
            trip_id: 42,
            ts:  1_700_000_300,
        });
    }

    #[test]
    fn roundtrip_ack() {
        roundtrip(Message::Ack);
    }

    #[test]
    fn roundtrip_position() {
        roundtrip(Message::Position {
            token: "abc123".into(),
            trip_id: 42,
            lat: 45.0,
            lon: 9.0,
            ts: 123,
        });
    }

    #[test]
    fn roundtrip_stats() {
        roundtrip(Message::Stats {
            token: "abc123".into(),
            from_ts: 0,
            to_ts: 300,
        });
    }

    #[test]
    fn roundtrip_stats_result() {
        roundtrip(Message::StatsResult {
            username: "mario".into(),
            from_ts: 0,
            to_ts: 300,
            distance_m: 222.0,
            movement_secs: 120,
            pause_secs: 180,
            total_secs: 300,
            avg_speed_mps: 1.85,
            avg_speed_kmh: 6.66,
            points: 6,
        });
    }

    #[test]
    fn roundtrip_error() {
        roundtrip(Message::Error {
            code: "AUTH_FAILED".into(),
            message: "credenziali non valide".into(),
        });
    }

    #[test]
    fn roundtrip_chat_to_server() {
        roundtrip(Message::ChatToServer {
            token: "abc123".into(),
            text: "hello".into(),
        });
    }

    #[test]
    fn roundtrip_chat_from_server_broadcast() {
        roundtrip(Message::ChatFromServer {
            from: None,
            text: "annuncio".into(),
        });
    }

    #[test]
    fn roundtrip_chat_from_server_direct() {
        roundtrip(Message::ChatFromServer {
            from: Some("mario".into()),
            text: "ciao".into(),
        });
    }

    #[test]
    fn roundtrip_disconnect() {
        roundtrip(Message::Disconnect {
            token: "abc123".into(),
        });
    }

    #[test]
    fn register_wire_format() {
        let msg = Message::Register {
            username: "mario".into(),
            password: "hash".into(),
        };
        let json = encode(&msg).unwrap();
        assert!(json.contains("\"type\":\"REGISTER\""));
        assert!(json.contains("\"username\":\"mario\""));
    }

    #[test]
    fn ack_wire_format() {
        let json = encode(&Message::Ack).unwrap();
        assert_eq!(json, "{\"type\":\"ACK\"}");
    }

    #[test]
    fn chat_to_server_wire_format() {
        let msg = Message::ChatToServer {
            token: "abc123".into(),
            text: "ciao".into(),
        };
        let json = encode(&msg).unwrap();
        assert_eq!(
            json,
            "{\"type\":\"CHAT\",\"token\":\"abc123\",\"text\":\"ciao\"}"
        );
    }

    #[test]
    fn chat_from_server_direct_wire_format() {
        let msg = Message::ChatFromServer {
            from: Some("mario".into()),
            text: "ciao".into(),
        };
        let json = encode(&msg).unwrap();
        assert_eq!(
            json,
            "{\"type\":\"MESSAGE\",\"from\":\"mario\",\"text\":\"ciao\"}"
        );
    }

    #[test]
    fn chat_from_server_broadcast_wire_format() {
        let msg = Message::ChatFromServer {
            from: None,
            text: "annuncio".into(),
        };
        let json = encode(&msg).unwrap();
        assert_eq!(json, "{\"type\":\"MESSAGE\",\"text\":\"annuncio\"}");
    }

    #[test]
    fn validate_username_ok() {
        assert!(validate_username("mario").is_ok());
        assert!(validate_username("mario_99").is_ok());
        assert!(validate_username("mario-luigi").is_ok());
    }

    #[test]
    fn validate_username_rejects_bad() {
        assert!(validate_username("").is_err());
        assert!(validate_username(&"x".repeat(MAX_USERNAME_LEN + 1)).is_err());
        assert!(validate_username("mario@dev").is_err());
        assert!(validate_username("mario rossi").is_err());
    }

    #[test]
    fn validate_password_ok() {
        assert!(validate_password("secret").is_ok());
    }

    #[test]
    fn validate_password_rejects_bad() {
        assert!(validate_password("abc").is_err());
        assert!(validate_password(&"x".repeat(MAX_PASSWORD_LEN + 1)).is_err());
    }

    #[test]
    fn validate_chat_rejects_bad() {
        assert!(validate_chat("").is_err());
        assert!(validate_chat(&"x".repeat(MAX_CHAT_LEN + 1)).is_err());
    }
}
