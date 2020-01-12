use clap::{load_yaml, App};
use select::{document::Document, predicate::Name};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io::LineWriter;
use std::io::Write;
use std::rc::Rc;
use std::str::FromStr;
use url::{Position, Url};

#[derive(Hash)]
struct QueueLink {
    depth: usize,
    url: Url,
}

enum Message {
    QueueLink(QueueLink),
    Exit,
}

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// https://rust-lang-nursery.github.io/rust-cookbook/web/scraping.html
fn get_base_url(url: &Url, doc: &Document) -> Result<Url> {
    let base_tag_href = doc.find(Name("base")).filter_map(|n| n.attr("href")).nth(0);

    let base_url =
        base_tag_href.map_or_else(|| Url::parse(&url[..Position::BeforePath]), Url::parse)?;

    Ok(base_url)
}

const MAX_DEPTH: usize = 3;

type Map<K, V> = HashMap<K, Rc<RefCell<V>>>;

struct Snapshot {
    count: usize,
    values: Map<String, Snapshot>,
}

struct Root {
    values: Map<String, Snapshot>,
}

impl Root {
    pub fn new() -> Self {
        Self { values: Map::new() }
    }

    fn output(&self) {
        let file = File::create("output.html").unwrap();
        let mut file = LineWriter::new(file);

        enum StackEntry {
            Snapshot((String, String, Rc<RefCell<Snapshot>>)),
            Link(String, String),
            OpenUl,
            CloseUl,
            OpenLi,
            CloseLi,
        }

        file.write_all(b"<html><body>").unwrap();

        let mut stack: Vec<StackEntry> = self
            .values
            .iter()
            .map(|(k, v)| {
                if v.borrow().values.len() > 0 {
                    vec![StackEntry::Snapshot((k.clone(), k.clone(), Rc::clone(v)))]
                } else {
                    vec![StackEntry::Link(k.clone(), k.clone())]
                }
            })
            .flatten()
            .collect();

        loop {
            let top = stack.pop();

            if top.is_none() {
                break;
            }

            let top = top.unwrap();

            match top {
                StackEntry::OpenUl => file.write_all(b"<ul>").unwrap(),
                StackEntry::CloseUl => file.write_all(b"</ul>").unwrap(),
                StackEntry::OpenLi => file.write_all(b"<li>").unwrap(),
                StackEntry::CloseLi => file.write_all(b"</li>").unwrap(),
                StackEntry::Link(text, base) => file
                    .write_fmt(format_args!("<a href=\"http://{}\">{}</a>", base, text))
                    .unwrap(),
                StackEntry::Snapshot((segment, base, snapshot)) => {
                    let snapshot = snapshot.borrow();

                    if snapshot.values.len() == 0 {
                        if segment.len() != 0 {
                            file.write_fmt(format_args!("<a href=\"//{}\">{}</a>", base, segment))
                                .unwrap();
                        }
                        continue;
                    }

                    stack.push(StackEntry::CloseUl);

                    for (k, v) in snapshot.values.iter() {
                        stack.push(StackEntry::CloseLi);
                        stack.push(StackEntry::Snapshot((
                            k.clone(),
                            base.clone() + "/" + k,
                            Rc::clone(v),
                        )));
                        stack.push(StackEntry::OpenLi);
                    }

                    stack.push(StackEntry::OpenUl);
                    stack.push(StackEntry::Link(segment, base.clone()));
                }
            }
        }

        file.write_all(b"</body></html").unwrap();
    }

    fn len(&self) -> usize {
        let mut stack: Vec<Rc<RefCell<Snapshot>>> =
            self.values.iter().map(|(_k, v)| Rc::clone(v)).collect();
        let mut count: usize = 0;

        loop {
            let top = stack.pop();

            if top.is_none() {
                break;
            }

            let top = top.unwrap();
            let snapshot = top.borrow();

            if snapshot.values.len() == 0 {
                count += 1;
                continue;
            }

            for (_k, v) in snapshot.values.iter() {
                stack.push(Rc::clone(v));
            }
        }

        count
    }

    fn check(&mut self, url: &Url) -> bool {
        let mut ret = true;

        if let Some(host) = url.host_str() {
            let host = host.to_string();

            if !self.values.contains_key(&host) {
                self.values.insert(
                    host.clone(),
                    Rc::new(RefCell::new(Snapshot {
                        count: 0,
                        values: Map::new(),
                    })),
                );
                ret = false;
            }

            if let Some(snapshot) = self.values.get(&host) {
                if let Some(segments) = url.path_segments() {
                    let mut snapshot = Rc::clone(snapshot);

                    for segment in segments {
                        let next;

                        {
                            let mut snap = snapshot.borrow_mut();
                            let segment = segment.to_string();

                            if !snap.values.contains_key(&segment) {
                                ret = false;
                                snap.values.insert(
                                    segment.clone(),
                                    Rc::new(RefCell::new(Snapshot {
                                        count: 0,
                                        values: Map::new(),
                                    })),
                                );
                            }

                            if let Some(snap) = snap.values.get_mut(&segment) {
                                let mut snap_mut = snap.borrow_mut();
                                snap_mut.count += 1;
                                next = Rc::clone(snap);
                            } else {
                                break;
                            }
                        }

                        snapshot = next;
                    }
                }
            }
        }

        ret
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let yaml = load_yaml!("cli.yml");
    let matches = App::from_yaml(yaml).get_matches();

    let deadend: Vec<&str> = vec![
        "github.com",
        "rust-lang.org",
        "wikipedia.org",
        "docs.rs",
        "markmail.org",
        "archlinux.org",
        "archive.org",
        "youtube.com",
        "clipchamp.com",
        "twitter.com",
        "freedesktop.org",
        "t.co",
        "issues.",
        "atlassian.com",
    ];

    let mut visited = Root::new();

    let (send, recv) = crossbeam_channel::unbounded::<Message>();

    {
        let send = send.clone();
        std::thread::spawn(move || {
            println!("Press any key to stop...");
            let mut user_input = String::new();
            std::io::stdin().read_line(&mut user_input).ok();
            send.send(Message::Exit).unwrap();
        });
    }

    let max_depth = matches
        .value_of("depth")
        .and_then(|x| usize::from_str(x).ok())
        .unwrap_or(MAX_DEPTH);

    for url in matches.values_of("url").unwrap() {
        if let Ok(url) = Url::parse(url) {
            send.send(Message::QueueLink(QueueLink { depth: 0, url }))
                .unwrap();
        } else {
            eprintln!("Failed to parse url {}", url);
        }
    }

    let mut len = 0;

    for message in recv.iter() {
        match message {
            Message::QueueLink(QueueLink { depth, url }) => {
                println!("Depth: {}, link: {}", depth, url);

                let link = url.to_string();

                if visited.check(&url) {
                    continue;
                }

                len += 1;

                if url
                    .host_str()
                    .map(|x| deadend.iter().fold(false, |acc, y| acc || x.contains(y)))
                    .unwrap_or(false)
                    || depth >= max_depth
                {
                    continue;
                }

                let send = send.clone();
                tokio::spawn(async move {
                    if let Ok(url) = Url::parse(&link) {
                        if let Ok(response) = reqwest::get(url.as_ref()).await {
                            if let Ok(body) = response.text().await {
                                let document = Document::from(body.as_str());
                                if let Ok(base_url) = get_base_url(&url, &document) {
                                    let base_parser = Url::options().base_url(Some(&base_url));

                                    for url in document
                                        .find(Name("a"))
                                        .filter_map(|link| link.attr("href"))
                                        .filter(|link| !link.contains("#"))
                                        .filter_map(|link| base_parser.parse(link).ok())
                                        .filter(|url| url.scheme().contains("http"))
                                    {
                                        send.send(Message::QueueLink(QueueLink {
                                            depth: depth + 1,
                                            url,
                                        }))
                                        .unwrap();
                                    }
                                }
                            }
                        }
                    }
                });
            }
            Message::Exit => {
                break;
            }
        }
    }

    visited.output();

    println!("Visited Len: {}", visited.len());
    println!("Counted Len: {}", len);
    println!("Cleaning up requests... please wait.");

    Ok(())
}
