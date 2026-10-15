use std::net::Ipv4Addr;

use anyhow::{Context, Result};
use hickory_proto::op::{Message, MessageType, ResponseCode};
use hickory_proto::rr::rdata::A;
use hickory_proto::rr::{RData, Record, RecordType};
use tokio::net::UdpSocket;
use tracing::{info, warn};

/// Runs a minimal DNS server that answers `A` queries for `*.<suffix>` with
/// `127.0.0.1` and REFUSEs everything else.
///
/// It deliberately never forwards or resolves anything outside `suffix`:
/// macOS's per-domain `/etc/resolver/<suffix>` file (see `alcazar setup`)
/// scopes which queries ever reach this server in the first place, so it
/// never needs to see, let alone answer for, unrelated domains.
/// Binds the (typically privileged, `:53`) listener. Split out from `serve`
/// so the daemon can do this while still root and drop privileges before
/// running the query loop.
pub async fn bind(listen: &str) -> Result<UdpSocket> {
    UdpSocket::bind(listen)
        .await
        .with_context(|| format!("binding DNS listener on {listen}"))
}

pub async fn serve(socket: UdpSocket, suffix: &str) -> Result<()> {
    let suffix = format!(".{}.", suffix.trim_matches('.').to_ascii_lowercase());
    info!(suffix = %suffix, "DNS server listening");

    let mut buf = [0u8; 512];
    loop {
        let (len, peer) = match socket.recv_from(&mut buf).await {
            Ok(x) => x,
            Err(e) => {
                warn!(error = %e, "DNS recv failed");
                continue;
            }
        };

        match handle_query(&buf[..len], &suffix) {
            Ok(response) => {
                if let Err(e) = socket.send_to(&response, peer).await {
                    warn!(error = %e, %peer, "DNS send failed");
                }
            }
            Err(e) => warn!(error = %e, %peer, "DNS query handling failed"),
        }
    }
}

fn handle_query(buf: &[u8], suffix: &str) -> Result<Vec<u8>> {
    let request = Message::from_vec(buf).context("decoding DNS query")?;

    let mut response = Message::new(
        request.metadata.id,
        MessageType::Response,
        request.metadata.op_code,
    );
    response.metadata.recursion_desired = request.metadata.recursion_desired;
    response.metadata.authoritative = true;

    let Some(query) = request.queries.first().cloned() else {
        response.metadata.response_code = ResponseCode::FormErr;
        return response.to_vec().context("encoding DNS response");
    };
    response.add_query(query.clone());

    // Own the trailing-dot ascii name once so both the suffix check and the
    // answer record can reuse it.
    let name = query.name().to_ascii().to_ascii_lowercase();
    let dotted = format!(".{name}");
    if !dotted.ends_with(suffix) && name != suffix.trim_start_matches('.') {
        response.metadata.response_code = ResponseCode::Refused;
        return response.to_vec().context("encoding DNS response");
    }

    if query.query_type() == RecordType::A {
        let record = Record::from_rdata(query.name().clone(), 60, RData::A(A(Ipv4Addr::LOCALHOST)));
        response.add_answer(record);
    }
    // Any other query type for a name we own gets an empty NOERROR answer
    // (correct DNS behavior for "name exists, no records of this type"),
    // rather than NXDOMAIN.

    response.to_vec().context("encoding DNS response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::op::{OpCode, Query};
    use hickory_proto::rr::{DNSClass, Name};

    fn build_query(name: &str, query_type: RecordType) -> Vec<u8> {
        let mut msg = Message::new(1234, MessageType::Query, OpCode::Query);
        msg.metadata.recursion_desired = true;
        let mut q = Query::new();
        q.set_name(Name::from_ascii(name).unwrap())
            .set_query_type(query_type)
            .set_query_class(DNSClass::IN);
        msg.add_query(q);
        msg.to_vec().unwrap()
    }

    #[test]
    fn resolves_a_record_within_suffix() {
        let query = build_query("api.myapp.test.", RecordType::A);
        let response_bytes = handle_query(&query, ".test.").unwrap();
        let response = Message::from_vec(&response_bytes).unwrap();
        assert_eq!(response.metadata.response_code, ResponseCode::NoError);
        assert_eq!(response.answers.len(), 1);
        match &response.answers[0].data {
            RData::A(a) => assert_eq!(a.0, Ipv4Addr::LOCALHOST),
            other => panic!("expected A record, got {other:?}"),
        }
    }

    #[test]
    fn refuses_names_outside_suffix() {
        let query = build_query("example.com.", RecordType::A);
        let response_bytes = handle_query(&query, ".test.").unwrap();
        let response = Message::from_vec(&response_bytes).unwrap();
        assert_eq!(response.metadata.response_code, ResponseCode::Refused);
    }

    #[test]
    fn empty_answers_for_non_a_queries_within_suffix() {
        let query = build_query("api.myapp.test.", RecordType::AAAA);
        let response_bytes = handle_query(&query, ".test.").unwrap();
        let response = Message::from_vec(&response_bytes).unwrap();
        assert_eq!(response.metadata.response_code, ResponseCode::NoError);
        assert!(response.answers.is_empty());
    }
}
