//! 中身のハッシュ（blake3）。ファイルは中身、フォルダは子から作る Merkle（Git の tree と同じ考え方）。
//!
//! `hash_drive` が、DB に残った木を下からたどって（深さ優先、名前順）、ハッシュの無いファイルを
//! 並列にハッシュし、子がそろったフォルダのハッシュを作って DB に書く。止めても、次は続きから。

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::db::{self, Db, Stored};
use crate::listing::Kind;

/// ハッシュのアルゴリズム（DB の `algo`）。
pub const ALGO: &str = "blake3";
/// 確定する間隔。
const COMMIT_EVERY: Duration = Duration::from_millis(300);
/// 一度に読む大きさ。
const BUFFER: usize = 1 << 20;

pub type Hash = [u8; 32];

/// ファイルの中身のハッシュ。
pub fn hash_file(path: &Path) -> io::Result<Hash> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; BUFFER];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

/// フォルダのハッシュの元になる子の 1 件。
pub struct Child<'a> {
    pub kind: Kind,
    pub name_nfc: &'a str,
    /// 中身のハッシュ。無いもの（クラウドのみ、リンク、読めなかったもの、別のボリューム）は None で、
    /// 代わりに大きさを入れる。
    pub hash: Option<&'a Hash>,
    pub size: u64,
}

/// フォルダのハッシュ。子は名前順（`name::hash_order`）で渡す。
pub fn folder_hash(children: &[Child]) -> Hash {
    let mut hasher = blake3::Hasher::new();
    for child in children {
        let kind = match child.kind {
            Kind::File => b'f',
            Kind::Dir => b'd',
            Kind::Symlink => b'l',
            Kind::Other => b'o',
        };
        hasher.update(&[kind]);
        hasher.update(&(child.name_nfc.len() as u64).to_le_bytes());
        hasher.update(child.name_nfc.as_bytes());
        match child.hash {
            Some(hash) => {
                hasher.update(&[1]);
                hasher.update(hash);
            }
            None => {
                hasher.update(&[0]);
                hasher.update(&child.size.to_le_bytes());
            }
        }
    }
    *hasher.finalize().as_bytes()
}

/// ハッシュの進み具合。
pub enum Event<'a> {
    /// このフォルダの中のファイルをハッシュし始める。
    Folder(&'a Path),
    /// ファイルを 1 つハッシュした（読めなかったときも含む）。
    File { bytes: u64 },
    /// フォルダのハッシュができた（中のファイルもそろった）。
    FolderDone(&'a Path),
    /// ここまでを DB に確定した。
    Committed,
}

/// ドライブ `drive_id`（`mount` につないである）の、ハッシュの無いものをハッシュする。
/// ファイルは `threads` 本で並列に読む。`stop` が true を返したらやめる。
pub fn hash_drive(
    db: &mut Db,
    drive_id: &str,
    mount: &Path,
    threads: usize,
    on: &mut dyn FnMut(Event),
    stop: &dyn Fn() -> bool,
) -> db::Result<()> {
    let Some(root) = db.drive_root(drive_id)? else {
        return Ok(());
    };
    let mut walker = Walker {
        db,
        threads: threads.max(1),
        on,
        stop,
        last_commit: Instant::now(),
    };
    walker.db.begin()?;
    walker.visit(&root, mount.to_path_buf())?;
    walker.db.commit()?;
    (walker.on)(Event::Committed);
    Ok(())
}

struct Walker<'a> {
    db: &'a mut Db,
    threads: usize,
    on: &'a mut dyn for<'p> FnMut(Event<'p>),
    stop: &'a dyn Fn() -> bool,
    last_commit: Instant,
}

impl Walker<'_> {
    /// フォルダ `folder` をハッシュする。できたら（下が全部そろったら）ハッシュを返す。
    fn visit(&mut self, folder: &Stored, path: PathBuf) -> db::Result<Option<Hash>> {
        if let Some(hash) = folder.hash {
            return Ok(Some(hash));
        }
        if !folder.listed || folder.error.is_some() || (self.stop)() {
            return Ok(None);
        }
        let mut children = self.db.children(folder.id)?;
        let mut complete = true;
        // 先に子のフォルダ（名前順）。
        for child in children.iter_mut() {
            if child.kind == Kind::Dir && !child.cloud_only && !child.other_volume {
                let child_path = path.join(crate::name::from_raw_bytes(&child.name_raw));
                match self.visit(child, child_path)? {
                    Some(hash) => child.hash = Some(hash),
                    // 読めなかったフォルダは、無いものとして親を作る（いつまでも親が決まらないので）。
                    None if child.error.is_some() => {}
                    None => complete = false,
                }
            }
        }
        if (self.stop)() {
            return Ok(None);
        }
        // 次に、このフォルダのファイルを並列に。
        let files: Vec<usize> = (0..children.len())
            .filter(|&i| {
                let c = &children[i];
                c.kind == Kind::File && !c.cloud_only && c.hash.is_none() && c.error.is_none()
            })
            .collect();
        if !files.is_empty() {
            (self.on)(Event::Folder(&path));
            let results = self.hash_files(&children, &files, &path);
            for (i, result) in files.into_iter().zip(results) {
                let child = &mut children[i];
                match result {
                    Ok(hash) => {
                        self.db.set_hash(child.id, &hash, child.size, child.mtime)?;
                        child.hash = Some(hash);
                    }
                    Err(error) => {
                        let error = crate::listing::Failure::of(&error);
                        self.db.set_hash_error(child.id, &format!("{error:?}"))?;
                    }
                }
                (self.on)(Event::File { bytes: child.size });
            }
        }
        if !complete {
            self.maybe_commit()?;
            return Ok(None);
        }
        let parts: Vec<Child> = children
            .iter()
            .map(|c| Child {
                kind: c.kind,
                name_nfc: &c.name_nfc,
                hash: c.hash.as_ref(),
                size: c.size,
            })
            .collect();
        let hash = folder_hash(&parts);
        self.db.set_hash(folder.id, &hash, 0, folder.mtime)?;
        (self.on)(Event::FolderDone(&path));
        self.maybe_commit()?;
        Ok(Some(hash))
    }

    fn hash_files(
        &self,
        children: &[Stored],
        files: &[usize],
        dir: &Path,
    ) -> Vec<io::Result<Hash>> {
        let paths: Vec<PathBuf> = files
            .iter()
            .map(|&i| dir.join(crate::name::from_raw_bytes(&children[i].name_raw)))
            .collect();
        if paths.len() == 1 || self.threads == 1 {
            return paths.iter().map(|p| hash_file(p)).collect();
        }
        let next = AtomicUsize::new(0);
        let results: Mutex<Vec<Option<io::Result<Hash>>>> =
            Mutex::new((0..paths.len()).map(|_| None).collect());
        std::thread::scope(|scope| {
            for _ in 0..self.threads.min(paths.len()) {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(path) = paths.get(i) else { break };
                        let result = hash_file(path);
                        results.lock().expect("結果")[i] = Some(result);
                    }
                });
            }
        });
        results
            .into_inner()
            .expect("結果")
            .into_iter()
            .map(|r| r.expect("全部ハッシュした"))
            .collect()
    }

    fn maybe_commit(&mut self) -> db::Result<()> {
        if self.last_commit.elapsed() >= COMMIT_EVERY {
            self.db.commit()?;
            (self.on)(Event::Committed);
            self.db.begin()?;
            self.last_commit = Instant::now();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listing::read_dir;
    use crate::volumes::Volume;
    use std::fs;

    #[test]
    fn same_contents_give_the_same_folder_hash() {
        let tmp = std::env::temp_dir().join(format!("lisa-hash-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        for dir in ["a", "b", "a/sub", "b/sub"] {
            fs::create_dir_all(tmp.join(dir)).unwrap();
        }
        for dir in ["a", "b"] {
            fs::write(tmp.join(dir).join("x.txt"), b"hello").unwrap();
            fs::write(tmp.join(dir).join("sub/y.txt"), b"world").unwrap();
        }
        let volume = Volume {
            id: Some("test:h".into()),
            label: "t".into(),
            fs: String::new(),
            mount_path: tmp.clone(),
            capacity: None,
            available: None,
            removable: false,
            network: false,
            read_only: false,
            case_sensitive: None,
            hidden: false,
        };
        let mut db = Db::open_in_memory().unwrap();
        let device = db.this_device("d", "d", "macos").unwrap();
        db.upsert_drive(&device, &volume).unwrap();
        let mut stack = vec![tmp.clone()];
        while let Some(path) = stack.pop() {
            let listing = read_dir(&path).map_err(|e| e.to_string());
            if let Ok(l) = &listing {
                for e in &l.entries {
                    if e.kind == Kind::Dir {
                        stack.push(path.join(&e.name));
                    }
                }
            }
            db.save_listing("test:h", &tmp, &path, &listing).unwrap();
        }
        let mut folders = Vec::new();
        let mut files = 0;
        hash_drive(
            &mut db,
            "test:h",
            &tmp,
            4,
            &mut |event| match event {
                Event::FolderDone(path) => folders.push(path.to_path_buf()),
                Event::File { .. } => files += 1,
                _ => {}
            },
            &|| false,
        )
        .unwrap();
        assert_eq!(files, 4);
        // 下から順に: a/sub、a、b/sub、b、一番上。
        assert_eq!(
            folders,
            [
                tmp.join("a/sub"),
                tmp.join("a"),
                tmp.join("b/sub"),
                tmp.join("b"),
                tmp.clone()
            ]
        );
        let root = db.drive_root("test:h").unwrap().unwrap();
        assert!(root.hash.is_some());
        let top = db.children(root.id).unwrap();
        assert_eq!(top[0].hash, top[1].hash);
        assert_eq!(
            db.children(top[0].id).unwrap()[1].hash,
            Some(*blake3::hash(b"hello").as_bytes())
        );
        // もう一度やっても、何もしない。
        let mut again = 0;
        hash_drive(
            &mut db,
            "test:h",
            &tmp,
            4,
            &mut |e| {
                if matches!(e, Event::File { .. }) {
                    again += 1
                }
            },
            &|| false,
        )
        .unwrap();
        assert_eq!(again, 0);
        let _ = fs::remove_dir_all(&tmp);
    }
}
