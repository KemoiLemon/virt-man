//! dnsmasq's query log, read for the access log's DNS half.
//!
//! dnsmasq answers DNS for the managed networks now, so there is no in-process
//! resolver left to report a lookup from. It does log every query it answers
//! (`log-queries=extra`, one file per network), and that log is the record:
//! the guest, the name it asked for, and the addresses that came back.
//!
//! The log is tailed with a byte offset rather than re-read, because it is a
//! file dnsmasq appends to for the life of the host and the supervisor's pass
//! runs every few seconds.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::net::Ipv4Addr;
use std::path::Path;

/// Per pass. A tail normally reads a handful of lines; this only bounds the
/// allocation when the daemon was stopped (or the pass skipped) for a long
/// time, and the rest is read on the next pass.
const MAX_READ: u64 = 4 << 20;
/// Unanswered queries kept per name. A query whose reply never came would
/// otherwise grow the queue for the life of the process.
const PENDING_PER_NAME: usize = 8;

/// One interesting line of the query log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// `query[A] example.com from 192.168.1.10`, also in its
    /// `log-queries=extra` form, prefixed with the query id and client:
    /// `1 192.168.1.10/1234 query[A] example.com from 192.168.1.10`.
    Query { client: Ipv4Addr, name: String },
    /// `reply example.com is 93.184.216.34` or `cached example.com is
    /// 93.184.216.34`. Anything after `is` that is not an IPv4 address (a
    /// CNAME, an AAAA record, NXDOMAIN) is not an answer to attach to a name.
    Answer { name: String, ip: Ipv4Addr },
}

/// An answer this daemon can attribute to a guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub client: Ipv4Addr,
    pub name: String,
    pub ip: Ipv4Addr,
    /// The first answer to a query this daemon saw. A further answer to the
    /// same query (a second A record) only names an address: one lookup is
    /// one row in the access log.
    pub first: bool,
}

fn valid_name(name: &str) -> bool {
    // A host name is at most 253 bytes; anything else on a `query` line is a
    // log line this parser should not carry into the access log.
    !name.is_empty() && name.len() <= 253 && name.chars().all(|c| c.is_ascii_graphic())
}

/// The record a log line holds, if any. Both dnsmasq formats are accepted: the
/// plain one starts with `query[`/`reply`/`cached` after the syslog prefix,
/// the `extra` one puts `<id> <client>/<port>` in front of it. Only A queries
/// are followed: an AAAA or CNAME answer is not an address a connection is
/// made to, and queuing one would leave an entry that no answer ever takes.
pub fn parse_line(line: &str) -> Option<Record> {
    let t: Vec<&str> = line.split_whitespace().collect();
    for (i, tok) in t.iter().enumerate() {
        if *tok == "query[A]" {
            let name = *t.get(i + 1)?;
            if !valid_name(name) {
                return None;
            }
            let rest = t.get(i + 2..)?;
            let from = rest.iter().position(|s| *s == "from")?;
            let client = t.get(i + 3 + from)?.parse().ok()?;
            return Some(Record::Query {
                client,
                name: name.to_string(),
            });
        }
        if *tok == "reply" || *tok == "cached" {
            let name = *t.get(i + 1)?;
            if !valid_name(name) || *t.get(i + 2)? != "is" {
                return None;
            }
            let ip = t.get(i + 3)?.parse().ok()?;
            return Some(Record::Answer {
                name: name.to_string(),
                ip,
            });
        }
    }
    None
}

/// One network's read position and the queries still waiting for an answer.
#[derive(Default)]
pub struct Tail {
    /// Byte offset after the last complete line read. `None` until the file is
    /// first seen.
    offset: Option<u64>,
    /// name -> queries waiting for an answer, oldest first. In the plain log
    /// format a reply carries no client, so an answer goes to the oldest.
    pending: HashMap<String, VecDeque<(Ipv4Addr, u64)>>,
    /// name -> the client of the query answered last. A further answer to it,
    /// or a `cached` reply to a query logged before this daemon started, still
    /// names the address without a second DNS row.
    last: HashMap<String, (Ipv4Addr, u64)>,
}

impl Tail {
    /// Read what was appended since the last call and pair it. `now` is when
    /// these lines are seen, which is close enough to when they were written.
    pub fn read(&mut self, path: &Path, now: u64) -> Vec<Answer> {
        let mut f = match File::open(path) {
            Ok(f) => f,
            // A missing log means nothing was logged yet, so read it from the
            // start when dnsmasq creates it. Any other failure is retried from
            // the same offset.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.offset.get_or_insert(0);
                return Vec::new();
            }
            Err(_) => return Vec::new(),
        };
        let Ok(len) = f.metadata().map(|m| m.len()) else {
            return Vec::new();
        };
        let start = match self.offset {
            None => {
                // First sight of the file. Start at its end: replaying a log
                // this daemon never read would put every lookup from before it
                // was watching into the access log, attributed to "now".
                self.offset = Some(len);
                return Vec::new();
            }
            // Rotation renames the log aside and starts a fresh empty file, so
            // a length below the offset is the signal; the new file would have
            // to grow past the old offset between two passes to be missed,
            // which the 64 MiB rotation cap makes impossible.
            Some(offset) if offset > len => 0,
            Some(offset) => offset,
        };
        if start >= len {
            return Vec::new();
        }
        let want = (len - start).min(MAX_READ) as usize;
        let mut buf = vec![0u8; want];
        if f.seek(SeekFrom::Start(start)).is_err() {
            return Vec::new();
        }
        let Ok(n) = f.read(&mut buf) else {
            return Vec::new();
        };
        buf.truncate(n);
        // Whole lines only: the offset stays before a partial tail, so a read
        // that lands mid-line never parses half a name or half an address.
        let Some(end) = buf.iter().rposition(|b| *b == b'\n') else {
            return Vec::new();
        };
        self.offset = Some(start + end as u64 + 1);
        let text = String::from_utf8_lossy(&buf[..=end]);
        let mut out = Vec::new();
        for line in text.lines() {
            self.feed(line, now, &mut out);
        }
        self.prune(now);
        out
    }

    fn feed(&mut self, line: &str, now: u64, out: &mut Vec<Answer>) {
        match parse_line(line) {
            Some(Record::Query { client, name }) => {
                let q = self.pending.entry(name).or_default();
                q.push_back((client, now));
                while q.len() > PENDING_PER_NAME {
                    q.pop_front();
                }
            }
            Some(Record::Answer { name, ip }) => {
                let answered = self.pending.get_mut(&name).and_then(|q| q.pop_front());
                let first = answered.is_some();
                let client = match answered {
                    Some((client, _)) => {
                        self.last.insert(name.clone(), (client, now));
                        client
                    }
                    // A query from before this daemon's first read, or a
                    // second A record: no row, but the address is named.
                    None => match self.last.get(&name) {
                        Some((client, _)) => *client,
                        None => return,
                    },
                };
                out.push(Answer {
                    client,
                    name,
                    ip,
                    first,
                });
            }
            None => {}
        }
    }

    /// Both maps hold what the guest asked for, so they are bounded the same
    /// way the flows' name map is. Cheap: mostly empty.
    fn prune(&mut self, now: u64) {
        let ttl = super::flows::NAME_TTL_MS;
        self.last.retain(|_, (_, at)| now.saturating_sub(*at) < ttl);
        self.pending.retain(|_, q| {
            q.retain(|(_, at)| now.saturating_sub(*at) < ttl);
            !q.is_empty()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn query(line: &str) -> (Ipv4Addr, String) {
        match parse_line(line) {
            Some(Record::Query { client, name }) => (client, name),
            other => panic!("expected a query, got {other:?}"),
        }
    }

    fn answer(line: &str) -> (String, Ipv4Addr) {
        match parse_line(line) {
            Some(Record::Answer { name, ip }) => (name, ip),
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    #[test]
    fn both_query_formats_are_read() {
        let (client, name) =
            query("Oct 18 12:00:00 dnsmasq[9]: query[A] example.com from 192.168.1.10");
        assert_eq!(
            (client.to_string(), name),
            ("192.168.1.10".into(), "example.com".into())
        );
        let (client, name) = query(
            "Oct 18 12:00:00 dnsmasq[9]: 1 192.168.1.10/1234 query[A] example.com from 192.168.1.10",
        );
        assert_eq!(
            (client.to_string(), name),
            ("192.168.1.10".into(), "example.com".into())
        );
    }

    #[test]
    fn replies_and_cached_answers_are_read() {
        assert_eq!(
            answer("dnsmasq[9]: reply example.com is 93.184.216.34"),
            ("example.com".into(), "93.184.216.34".parse().unwrap())
        );
        assert_eq!(
            answer("dnsmasq[9]: 1 192.168.1.10/1234 cached example.com is 93.184.216.34"),
            ("example.com".into(), "93.184.216.34".parse().unwrap())
        );
    }

    #[test]
    fn non_address_answers_and_other_query_types_are_ignored() {
        for line in [
            "dnsmasq[9]: reply example.com is cname.example.net",
            "dnsmasq[9]: reply example.com is 2606:2800:220:1:248:1893:25c8:1946",
            "dnsmasq[9]: reply example.com is NODATA",
            "dnsmasq[9]: config example.com is 10.0.0.2",
            "dnsmasq[9]: query[AAAA] example.com from 192.168.1.10",
            "dnsmasq[9]: query[MX] example.com from 192.168.1.10",
        ] {
            assert_eq!(parse_line(line), None, "{line}");
        }
    }

    #[test]
    fn forwarding_and_dhcp_lines_are_not_records() {
        for line in [
            "dnsmasq[9]: 1 192.168.1.10/1234 forwarded example.com to 1.1.1.1",
            "dnsmasq-dhcp[9]: DHCPACK(br0) 192.168.1.5 52:54:00:aa:bb:cc web",
            "dnsmasq[9]: started, version 2.93 cachesize 150",
            "",
            "\u{1}\u{2}garbage is 1.2.3.4",
            "query[A] example.com from not-an-address",
            "reply is 1.2.3.4",
        ] {
            assert_eq!(parse_line(line), None, "{line}");
        }
    }

    fn file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dnslog-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("dnsmasq-n1.log")
    }

    fn append(path: &Path, text: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    /// The query and its reply as dnsmasq writes them, one per line.
    fn qa(name: &str, client: &str, ip: &str) -> String {
        format!(
            "dnsmasq[9]: 1 {client}/4242 query[A] {name} from {client}\n\
             dnsmasq[9]: 1 {client}/4242 reply {name} is {ip}\n"
        )
    }

    #[test]
    fn a_query_and_its_reply_become_one_answer() {
        let path = file("pair");
        append(&path, &qa("example.com", "10.9.0.7", "93.184.216.34"));
        let mut tail = Tail::default();
        assert!(
            tail.read(&path, 10).is_empty(),
            "the existing log is history"
        );
        append(&path, &qa("example.com", "10.9.0.7", "93.184.216.34"));
        assert_eq!(
            tail.read(&path, 20),
            vec![Answer {
                client: "10.9.0.7".parse().unwrap(),
                name: "example.com".into(),
                ip: "93.184.216.34".parse().unwrap(),
                first: true,
            }]
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_second_address_is_named_but_not_a_second_row() {
        let path = file("extra-answer");
        let mut tail = Tail::default();
        tail.read(&path, 0);
        append(
            &path,
            "dnsmasq[9]: query[A] example.com from 10.9.0.7\n\
             dnsmasq[9]: reply example.com is 1.2.3.4\n\
             dnsmasq[9]: reply example.com is 5.6.7.8\n",
        );
        let got = tail.read(&path, 20);
        assert_eq!(
            got.iter()
                .map(|a| (a.ip.to_string(), a.first))
                .collect::<Vec<_>>(),
            vec![("1.2.3.4".into(), true), ("5.6.7.8".into(), false)],
            "the first answer is the row, the second only names the address"
        );
        assert!(got.iter().all(|a| a.client.to_string() == "10.9.0.7"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Two guests asking for the same name: each answer goes to a client that
    /// asked, oldest query first.
    #[test]
    fn answers_pair_with_the_oldest_pending_query() {
        let path = file("clients");
        let mut tail = Tail::default();
        tail.read(&path, 0);
        append(
            &path,
            "dnsmasq[9]: query[A] example.com from 10.9.0.7\n\
             dnsmasq[9]: query[A] example.com from 10.9.0.8\n\
             dnsmasq[9]: reply example.com is 1.2.3.4\n\
             dnsmasq[9]: reply example.com is 1.2.3.4\n",
        );
        let got = tail.read(&path, 5);
        assert_eq!(
            got.iter().map(|a| a.client.to_string()).collect::<Vec<_>>(),
            vec!["10.9.0.7", "10.9.0.8"]
        );
        assert!(got.iter().all(|a| a.first));

        // A reply with nothing pending is still a named address, once a query
        // for that name was seen; with no client at all it is dropped.
        append(
            &path,
            "dnsmasq[9]: cached example.com is 1.2.3.4\n\
             dnsmasq[9]: cached unknown.example is 1.1.1.1\n",
        );
        let got = tail.read(&path, 6);
        assert_eq!(got.len(), 1);
        assert_eq!(
            (
                got[0].name.as_str(),
                got[0].first,
                got[0].client.to_string()
            ),
            ("example.com", false, "10.9.0.8".to_string())
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A reply may arrive after the pass that read its query: the pairing
    /// state belongs to the tail, not to one read.
    #[test]
    fn a_reply_in_the_next_pass_pairs_with_the_earlier_query() {
        let path = file("next-pass");
        let mut tail = Tail::default();
        tail.read(&path, 0);
        append(&path, "dnsmasq[9]: query[A] example.com from 10.9.0.7\n");
        assert!(tail.read(&path, 1).is_empty());
        append(&path, "dnsmasq[9]: reply example.com is 1.2.3.4\n");
        let got = tail.read(&path, 2);
        assert_eq!(got.len(), 1);
        assert!(got[0].first);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_line_is_read_once_and_a_shortened_file_again() {
        let path = file("offset");
        let mut tail = Tail::default();
        append(&path, &qa("one.example", "10.9.0.7", "1.2.3.4"));
        // First sight starts at the end: what was already there is history.
        assert!(tail.read(&path, 0).is_empty());
        let first = "dnsmasq[9]: query[A] two.example from 10.9.0.7\n\
                     dnsmasq[9]: reply two.example is 5.6.7.8\n";
        append(&path, first);
        assert_eq!(tail.read(&path, 1).len(), 1);
        assert!(tail.read(&path, 2).is_empty(), "no line is read twice");
        // A partial line is left for the next pass, not parsed.
        append(&path, "dnsmasq[9]: query[A] three.example from ");
        assert!(tail.read(&path, 3).is_empty());
        append(
            &path,
            "10.9.0.7\ndnsmasq[9]: reply three.example is 9.9.9.9\n",
        );
        assert_eq!(tail.read(&path, 4)[0].name, "three.example");
        // Truncated (or rotated to a fresh file): read again from the start,
        // and the pairing state of the old file is not reused as new rows.
        std::fs::write(&path, "dnsmasq[9]: query[A] four.example from 10.9.0.7\n").unwrap();
        assert!(tail.read(&path, 5).is_empty());
        append(&path, "dnsmasq[9]: reply four.example is 8.8.8.8\n");
        let got = tail.read(&path, 6);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "four.example");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_missing_file_is_not_an_error_and_nothing_before_it_is_replayed() {
        let mut tail = Tail::default();
        assert!(
            tail.read(Path::new("/nonexistent/dnsmasq-n1.log"), 0)
                .is_empty()
        );
        // A log that did not exist when the tail first looked has no history
        // to skip, so what dnsmasq writes from then on is read.
        let path = file("missing-then-there");
        assert!(tail.read(&path, 1).is_empty());
        append(&path, &qa("one.example", "10.9.0.7", "1.2.3.4"));
        assert_eq!(tail.read(&path, 2).len(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
