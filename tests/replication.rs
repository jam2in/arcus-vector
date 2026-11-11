//! The vop commands on a replication group: writes on the master reach the slave's index.
//!
//! Runs against the master and slave at `ARCUS_VECTOR_MASTER` and `ARCUS_VECTOR_SLAVE`, both
//! loaded with this extension; without them it skips, unless `CI` is set.
#![expect(
    clippy::unwrap_used,
    reason = "the helpers fail the test that calls them on any error"
)]
#![expect(clippy::print_stderr, reason = "a skip says why")]

use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// How long the slave may take to catch up with the master.
const CATCH_UP: Duration = Duration::from_secs(10);

/// A search hit: the vkey, its score as the server wrote it, and its metadata if asked for.
type Hit = (String, String, Option<String>);

#[derive(Debug, PartialEq)]
enum Reply {
    Line(String),
    Meta(String),
    Vectors(Vec<Hit>),
}

struct Conn {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Conn {
    fn new(addr: &str) -> Self {
        let writer = TcpStream::connect(addr).unwrap();
        writer.set_read_timeout(Some(CATCH_UP)).unwrap();
        let reader = BufReader::new(writer.try_clone().unwrap());
        Self { reader, writer }
    }

    /// Sends `line` and each of `data` as a line after it, and reads the reply.
    fn call(&mut self, line: &str, data: &[&[u8]]) -> Reply {
        let mut request = format!("{line}\r\n").into_bytes();
        for d in data {
            request.extend_from_slice(d);
            request.extend_from_slice(b"\r\n");
        }
        self.writer.write_all(&request).unwrap();
        let first = self.line();
        let words: Vec<&str> = first.split(' ').collect();
        match words[..] {
            ["META", _, len] => Reply::Meta(self.block(len)),
            ["VECTORS", count] => {
                let hits = (0..count.parse().unwrap())
                    .map(|_| {
                        let hit = self.line();
                        match hit.split(' ').collect::<Vec<_>>()[..] {
                            [vkey, score] => (vkey.to_owned(), score.to_owned(), None),
                            [vkey, score, len] => {
                                (vkey.to_owned(), score.to_owned(), Some(self.block(len)))
                            }
                            _ => panic!("a hit line: {hit}"),
                        }
                    })
                    .collect();
                assert_eq!(self.line(), "END");
                Reply::Vectors(hits)
            }
            _ => Reply::Line(first),
        }
    }

    fn line(&mut self) -> String {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        line.strip_suffix("\r\n").unwrap().to_owned()
    }

    fn block(&mut self, len: &str) -> String {
        let mut data = vec![0; len.parse::<usize>().unwrap() + 2];
        self.reader.read_exact(&mut data).unwrap();
        assert!(data.ends_with(b"\r\n"));
        data.truncate(data.len() - 2);
        String::from_utf8(data).unwrap()
    }

    fn create(&mut self, index: &str, dim: usize) {
        let line = format!("vop create {index} {dim} 0");
        assert_eq!(self.call(&line, &[]), line_of("CREATED"));
    }

    fn upsert(&mut self, index: &str, vkey: &str, vector: &[f32], meta: &str) -> Reply {
        let vector = bin(vector);
        let line = format!(
            "vop upsert {index} {vkey} {} meta {}",
            vector.len(),
            meta.len()
        );
        self.call(&line, &[&vector, meta.as_bytes()])
    }

    fn search(&mut self, index: &str, vector: &[f32], num: usize) -> Reply {
        let vector = bin(vector);
        let line = format!("vop search {index} vector {} {num} withmeta", vector.len());
        self.call(&line, &[&vector])
    }

    fn getmeta(&mut self, index: &str, vkey: &str) -> Reply {
        self.call(&format!("vop getmeta {index} {vkey}"), &[])
    }
}

fn line_of(line: &str) -> Reply {
    Reply::Line(line.to_owned())
}

fn bin(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// The master and the slave, or `None` outside a cluster.
fn cluster() -> Option<(Conn, Conn)> {
    match (
        env::var("ARCUS_VECTOR_MASTER"),
        env::var("ARCUS_VECTOR_SLAVE"),
    ) {
        (Ok(master), Ok(slave)) => Some((Conn::new(&master), Conn::new(&slave))),
        _ => {
            assert!(
                env::var_os("CI").is_none(),
                "CI runs these against a cluster: set ARCUS_VECTOR_MASTER and ARCUS_VECTOR_SLAVE"
            );
            eprintln!("no ARCUS_VECTOR_MASTER and ARCUS_VECTOR_SLAVE: skipped");
            None
        }
    }
}

/// An index name of `tag` no earlier run on the cluster has used.
fn fresh(tag: &str) -> String {
    let nanos = SystemTime::UNIX_EPOCH.elapsed().unwrap().as_nanos();
    format!("repl_{tag}_{}_{nanos}", process::id())
}

/// Waits until `read` gives `want`, and fails with what it last gave after [`CATCH_UP`].
fn eventually(want: &Reply, mut read: impl FnMut() -> Reply) {
    let deadline = Instant::now() + CATCH_UP;
    loop {
        let got = read();
        if got == *want {
            return;
        }
        assert!(Instant::now() < deadline, "want {want:?}, got {got:?}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// A vector of `dim` dimensions for each `i`, scattered so that few are near another.
fn vector(i: usize, dim: usize) -> Vec<f32> {
    (0..dim)
        .map(|d| ((i * 2_654_435_761 + d * 40_503) % 1_000) as f32 / 100.0 - 5.0)
        .collect()
}

/// `reply` with its hits in vkey order, as replicas may order equal scores apart.
fn sorted(reply: Reply) -> Reply {
    match reply {
        Reply::Vectors(mut hits) => {
            hits.sort();
            Reply::Vectors(hits)
        }
        other => other,
    }
}

#[test]
fn a_created_index_appears_on_the_slave() {
    let Some((mut master, mut slave)) = cluster() else {
        return;
    };
    let index = fresh("create");
    assert_eq!(slave.search(&index, &[1.0; 4], 1), line_of("NOT_FOUND"));
    master.create(&index, 4);
    eventually(&Reply::Vectors(vec![]), || {
        slave.search(&index, &[1.0; 4], 1)
    });
    assert_eq!(
        master.call(&format!("vop create {index} 4 0"), &[]),
        line_of("EXISTS")
    );
}

#[test]
fn upserts_reach_the_slave_with_their_vectors_and_meta() {
    let Some((mut master, mut slave)) = cluster() else {
        return;
    };
    let index = fresh("upsert");
    master.create(&index, 4);
    for i in 0..20 {
        let meta = format!(r#"{{"n":{i}}}"#);
        let reply = master.upsert(&index, &format!("v{i}"), &vector(i, 4), &meta);
        assert_eq!(reply, line_of("STORED"));
    }
    let reply = master.upsert(&index, "v0", &vector(99, 4), r#"{"n":99}"#);
    assert_eq!(reply, line_of("REPLACED"));
    for query in [vector(0, 4), vector(7, 4), vector(99, 4)] {
        let want = sorted(master.search(&index, &query, 20));
        let Reply::Vectors(hits) = &want else {
            panic!("a search: {want:?}");
        };
        assert_eq!(hits.len(), 20);
        eventually(&want, || sorted(slave.search(&index, &query, 20)));
    }
    let want = master.getmeta(&index, "v0");
    assert_eq!(want, Reply::Meta(r#"{"n":99}"#.to_owned()));
    eventually(&want, || slave.getmeta(&index, "v0"));
}

#[test]
fn filtered_and_vkey_searches_on_the_slave_match_the_master() {
    let Some((mut master, mut slave)) = cluster() else {
        return;
    };
    let index = fresh("filter");
    master.create(&index, 4);
    for i in 0..20 {
        let meta = format!(r#"{{"n":{i},"cat":"{}"}}"#, ["a", "b"][i % 2]);
        master.upsert(&index, &format!("v{i}"), &vector(i, 4), &meta);
    }
    let all = sorted(master.search(&index, &vector(0, 4), 20));
    eventually(&all, || sorted(slave.search(&index, &vector(0, 4), 20)));
    let query = bin(&vector(3, 4));
    for (filter, count) in [("n < 10", 10), ("cat = a", 10), ("cat = b and n >= 15", 3)] {
        let line = format!(
            "vop search {index} vector {} 20 filter {} withmeta",
            query.len(),
            filter.len()
        );
        let want = sorted(master.call(&line, &[&query, filter.as_bytes()]));
        let Reply::Vectors(hits) = &want else {
            panic!("a search: {want:?}");
        };
        assert_eq!(hits.len(), count, "{filter}");
        assert_eq!(
            sorted(slave.call(&line, &[&query, filter.as_bytes()])),
            want
        );
        let line = format!(
            "vop search {index} vkey v3 20 filter {} withmeta",
            filter.len()
        );
        let want = sorted(master.call(&line, &[filter.as_bytes()]));
        assert_eq!(sorted(slave.call(&line, &[filter.as_bytes()])), want);
    }
    let line = format!("vop search {index} vkey v3 5 withmeta");
    let want = master.call(&line, &[]);
    let Reply::Vectors(hits) = &want else {
        panic!("a search: {want:?}");
    };
    assert_eq!(hits[0].0, "v3");
    assert_eq!(sorted(slave.call(&line, &[])), sorted(want));
}

#[test]
fn a_drop_and_a_create_again_reach_the_slave() {
    let Some((mut master, mut slave)) = cluster() else {
        return;
    };
    let index = fresh("drop");
    master.create(&index, 4);
    master.upsert(&index, "old", &vector(0, 4), "{}");
    eventually(&Reply::Meta("{}".to_owned()), || {
        slave.getmeta(&index, "old")
    });
    let drop = format!("vop drop {index}");
    assert_eq!(master.call(&drop, &[]), line_of("DROPPED"));
    eventually(&line_of("NOT_FOUND"), || slave.getmeta(&index, "old"));
    // Under the same name, an index of other dimensions.
    master.create(&index, 8);
    master.upsert(&index, "new", &vector(0, 8), "{}");
    let want = master.search(&index, &vector(0, 8), 5);
    eventually(&want, || slave.search(&index, &vector(0, 8), 5));
    assert_eq!(slave.getmeta(&index, "old"), line_of("NOT_FOUND_ELEMENT"));
}
