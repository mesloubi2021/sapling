use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use memmap::{Mmap, MmapOptions};
use std::fs::File;
use std::fs::remove_file;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ancestors::{AncestorIterator, AncestorTraversal};
use error::Result;
use historyindex::HistoryIndex;
use historystore::{Ancestors, HistoryStore, NodeInfo};
use key::Key;
use node::Node;
use repack::{IterableStore, RepackOutputType, Repackable};

#[derive(Debug, Fail)]
#[fail(display = "Historypack Error: {:?}", _0)]
struct HistoryPackError(String);

#[derive(Clone, Debug, PartialEq)]
pub enum HistoryPackVersion {
    Zero,
    One,
}

impl HistoryPackVersion {
    fn new(value: u8) -> Result<Self> {
        match value {
            0 => Ok(HistoryPackVersion::Zero),
            1 => Ok(HistoryPackVersion::One),
            _ => Err(HistoryPackError(format!(
                "invalid history pack version number '{:?}'",
                value
            )).into()),
        }
    }
}

impl From<HistoryPackVersion> for u8 {
    fn from(version: HistoryPackVersion) -> u8 {
        match version {
            HistoryPackVersion::Zero => 0,
            HistoryPackVersion::One => 1,
        }
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct FileSectionHeader<'a> {
    pub file_name: &'a [u8],
    pub count: u32,
}

#[derive(Debug, PartialEq)]
pub struct HistoryEntry<'a> {
    pub node: Node,
    pub p1: Node,
    pub p2: Node,
    pub link_node: Node,
    pub copy_from: Option<&'a [u8]>,
}

fn read_slice<'a, 'b>(cur: &'a mut Cursor<&[u8]>, buf: &'b [u8], size: usize) -> Result<&'b [u8]> {
    let start = cur.position() as usize;
    let end = start + size;
    let file_name = &buf.get(start..end).ok_or_else(|| {
        HistoryPackError(format!(
            "buffer (length {:?}) not long enough to read {:?} bytes",
            buf.len(),
            size
        ))
    })?;
    cur.set_position(end as u64);
    Ok(file_name)
}

impl<'a> FileSectionHeader<'a> {
    pub(crate) fn read(buf: &[u8]) -> Result<FileSectionHeader> {
        let mut cur = Cursor::new(buf);
        let file_name_len = cur.read_u16::<BigEndian>()? as usize;
        let file_name = read_slice(&mut cur, &buf, file_name_len)?;

        let count = cur.read_u32::<BigEndian>()?;
        Ok(FileSectionHeader { file_name, count })
    }

    pub fn write<T: Write>(&self, writer: &mut T) -> Result<()> {
        writer.write_u16::<BigEndian>(self.file_name.len() as u16)?;
        writer.write_all(self.file_name)?;
        writer.write_u32::<BigEndian>(self.count)?;
        Ok(())
    }
}

impl<'a> HistoryEntry<'a> {
    pub(crate) fn read(buf: &[u8]) -> Result<HistoryEntry> {
        let mut cur = Cursor::new(buf);
        let mut node_buf: [u8; 20] = Default::default();

        // Node
        cur.read_exact(&mut node_buf)?;
        let node = Node::from(&node_buf);

        // Parents
        cur.read_exact(&mut node_buf)?;
        let p1 = Node::from(&node_buf);
        cur.read_exact(&mut node_buf)?;
        let p2 = Node::from(&node_buf);

        // LinkNode
        cur.read_exact(&mut node_buf)?;
        let link_node = Node::from(&node_buf);

        // Copyfrom
        let copy_from_len = cur.read_u16::<BigEndian>()? as usize;
        let copy_from = if copy_from_len > 0 {
            Some(read_slice(&mut cur, &buf, copy_from_len)?)
        } else {
            None
        };

        Ok(HistoryEntry {
            node,
            p1,
            p2,
            link_node,
            copy_from,
        })
    }

    pub fn write<T: Write>(
        writer: &mut T,
        node: &Node,
        p1: &Node,
        p2: &Node,
        linknode: &Node,
        copy_from: &Option<&[u8]>,
    ) -> Result<()> {
        writer.write_all(node.as_ref())?;
        writer.write_all(p1.as_ref())?;
        writer.write_all(p2.as_ref())?;
        writer.write_all(linknode.as_ref())?;
        match copy_from {
            Some(file_name) => {
                writer.write_u16::<BigEndian>(file_name.len() as u16)?;
                writer.write_all(file_name)?;
            }
            None => writer.write_u16::<BigEndian>(0)?,
        };

        Ok(())
    }
}

pub struct HistoryPack {
    mmap: Mmap,
    version: HistoryPackVersion,
    index: HistoryIndex,
    base_path: Arc<PathBuf>,
    pack_path: PathBuf,
    index_path: PathBuf,
}

impl HistoryPack {
    pub fn new(path: &Path) -> Result<Self> {
        let base_path = PathBuf::from(path);
        let pack_path = path.with_extension("histpack");
        let file = File::open(&pack_path)?;
        let len = file.metadata()?.len();
        if len < 1 {
            return Err(format_err!(
                "empty histpack '{:?}' is invalid",
                path.to_str().unwrap_or("<unknown>")
            ));
        }

        let mmap = unsafe { MmapOptions::new().len(len as usize).map(&file)? };
        let version = HistoryPackVersion::new(mmap[0])?;
        let index_path = path.with_extension("histidx");
        Ok(HistoryPack {
            mmap: mmap,
            version: version,
            index: HistoryIndex::new(&index_path)?,
            base_path: Arc::new(base_path),
            pack_path: pack_path,
            index_path: index_path,
        })
    }

    pub fn len(&self) -> usize {
        self.mmap.len()
    }

    pub fn base_path(&self) -> &Path {
        &self.base_path
    }

    pub fn pack_path(&self) -> &Path {
        &self.pack_path
    }

    pub fn index_path(&self) -> &Path {
        &self.index_path
    }

    fn read_file_section_header(&self, offset: u64) -> Result<FileSectionHeader> {
        FileSectionHeader::read(&self.mmap.as_ref()[offset as usize..])
    }

    fn read_history_entry(&self, offset: u64) -> Result<HistoryEntry> {
        HistoryEntry::read(&self.mmap.as_ref()[offset as usize..])
    }

    fn read_node_info(&self, key: &Key, offset: u64) -> Result<NodeInfo> {
        let entry = self.read_history_entry(offset)?;
        assert_eq!(&entry.node, key.node());
        let p1 = Key::new(
            Box::from(match entry.copy_from {
                Some(value) => value,
                None => key.name(),
            }),
            entry.p1.clone(),
        );
        let p2 = Key::new(Box::from(key.name()), entry.p2.clone());

        Ok(NodeInfo {
            parents: [p1, p2],
            linknode: entry.link_node.clone(),
        })
    }
}

impl HistoryStore for HistoryPack {
    fn get_ancestors(&self, key: &Key) -> Result<Ancestors> {
        AncestorIterator::new(
            key,
            |k, _seen| self.get_node_info(k),
            AncestorTraversal::Partial,
        ).collect()
    }

    fn get_missing(&self, keys: &[Key]) -> Result<Vec<Key>> {
        Ok(keys.iter()
            .filter(|k| self.index.get_node_entry(k).is_err())
            .map(|k| k.clone())
            .collect())
    }

    fn get_node_info(&self, key: &Key) -> Result<NodeInfo> {
        let node_location = self.index.get_node_entry(key)?;
        self.read_node_info(key, node_location.offset)
    }
}

impl IterableStore for HistoryPack {
    fn iter<'a>(&'a self) -> Box<Iterator<Item = Result<Key>> + 'a> {
        Box::new(HistoryPackIterator::new(self))
    }
}

impl Repackable for HistoryPack {
    fn delete(&self) -> Result<()> {
        let result1 = remove_file(&self.pack_path);
        let result2 = remove_file(&self.index_path);
        // Only check for errors after both have run. That way if pack_path doesn't exist,
        // index_path is still deleted.
        result1?;
        result2?;
        Ok(())
    }

    fn id(&self) -> &Arc<PathBuf> {
        &self.base_path
    }

    fn kind(&self) -> RepackOutputType {
        RepackOutputType::History
    }
}

struct HistoryPackIterator<'a> {
    pack: &'a HistoryPack,
    offset: u64,
    current_name: Vec<u8>,
    current_remaining: u32,
}

impl<'a> HistoryPackIterator<'a> {
    pub fn new(pack: &'a HistoryPack) -> Self {
        HistoryPackIterator {
            pack: pack,
            offset: 1, // Start after the header byte
            current_name: vec![],
            current_remaining: 0,
        }
    }
}

impl<'a> Iterator for HistoryPackIterator<'a> {
    type Item = Result<Key>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.current_remaining == 0 && (self.offset as usize) < self.pack.len() {
            let file_header = self.pack.read_file_section_header(self.offset);
            match file_header {
                Ok(header) => {
                    self.current_name.clear();
                    self.current_name.extend_from_slice(header.file_name);
                    self.current_remaining = header.count;
                    self.offset += 4 + 2 + header.file_name.len() as u64;
                }
                Err(e) => {
                    return Some(Err(e));
                }
            };
        }

        if self.offset as usize >= self.pack.len() {
            return None;
        }

        let entry = self.pack.read_history_entry(self.offset);
        self.current_remaining -= 1;
        Some(match entry {
            Ok(ref e) => {
                self.offset += 80;
                self.offset += match e.copy_from {
                    Some(v) => 2 + v.len() as u64,
                    None => 2,
                };
                Ok(Key::new(
                    self.current_name.clone().into_boxed_slice(),
                    e.node,
                ))
            }
            Err(e) => Err(e),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::chacha::ChaChaRng;
    use std::collections::HashMap;
    use tempfile::TempDir;

    use mutablehistorypack::MutableHistoryPack;

    fn make_pack(tempdir: &TempDir, nodes: &HashMap<Key, NodeInfo>) -> HistoryPack {
        let mut mutpack = MutableHistoryPack::new(tempdir.path(), HistoryPackVersion::One).unwrap();
        for (ref key, ref info) in nodes.iter() {
            mutpack.add(key.clone(), info.clone()).unwrap();
        }

        let path = mutpack.close().unwrap();

        HistoryPack::new(&path).unwrap()
    }

    fn get_nodes(mut rng: &mut ChaChaRng) -> (HashMap<Key, NodeInfo>, HashMap<Key, Ancestors>) {
        let file1 = Box::new([1, 2, 3]);
        let file2 = Box::new([1, 2, 3, 4, 5]);
        let null = Node::null_id();
        let node1 = Node::random(&mut rng);
        let node2 = Node::random(&mut rng);
        let node3 = Node::random(&mut rng);
        let node4 = Node::random(&mut rng);
        let node5 = Node::random(&mut rng);
        let node6 = Node::random(&mut rng);

        let mut nodes = HashMap::new();
        let mut ancestor_map = HashMap::new();

        // Insert key 1
        let key1 = Key::new(file1.clone(), node2.clone());
        let info = NodeInfo {
            parents: [
                Key::new(file1.clone(), node1.clone()),
                Key::new(file1.clone(), null.clone()),
            ],
            linknode: Node::random(&mut rng),
        };
        nodes.insert(key1.clone(), info.clone());
        let mut ancestors = HashMap::new();
        ancestors.insert(key1.clone(), info.clone());
        ancestor_map.insert(key1.clone(), ancestors);

        // Insert key 2
        let key2 = Key::new(file2.clone(), node3.clone());
        let info = NodeInfo {
            parents: [
                Key::new(file2.clone(), node5.clone()),
                Key::new(file2.clone(), node6.clone()),
            ],
            linknode: Node::random(&mut rng),
        };
        nodes.insert(key2.clone(), info.clone());
        let mut ancestors = HashMap::new();
        ancestors.insert(key2.clone(), info.clone());
        ancestor_map.insert(key2.clone(), ancestors);

        // Insert key 3
        let key3 = Key::new(file1.clone(), node4.clone());
        let info = NodeInfo {
            parents: [key2.clone(), key1.clone()],
            linknode: Node::random(&mut rng),
        };
        nodes.insert(key3.clone(), info.clone());
        let mut ancestors = HashMap::new();
        ancestors.insert(key3.clone(), info.clone());
        ancestors.extend(ancestor_map.get(&key2).unwrap().clone());
        ancestors.extend(ancestor_map.get(&key1).unwrap().clone());
        ancestor_map.insert(key3.clone(), ancestors);

        (nodes, ancestor_map)
    }

    #[test]
    fn test_get_ancestors() {
        let mut rng = ChaChaRng::from_seed([0u8; 32]);
        let tempdir = TempDir::new().unwrap();

        let (nodes, ancestors) = get_nodes(&mut rng);

        let pack = make_pack(&tempdir, &nodes);

        for (ref key, ref info) in nodes.iter() {
            pack.get_node_info(key).unwrap();
            let response: Ancestors = pack.get_ancestors(key).unwrap();
            assert_eq!(&response, ancestors.get(key).unwrap());
        }
    }

    #[test]
    fn test_get_node_info() {
        let mut rng = ChaChaRng::from_seed([0u8; 32]);
        let tempdir = TempDir::new().unwrap();

        let (nodes, _) = get_nodes(&mut rng);

        let pack = make_pack(&tempdir, &nodes);

        for (ref key, ref info) in nodes.iter() {
            let response: NodeInfo = pack.get_node_info(key).unwrap();
            assert_eq!(response, **info);
        }
    }

    #[test]
    fn test_get_missing() {
        let mut rng = ChaChaRng::from_seed([0u8; 32]);
        let tempdir = TempDir::new().unwrap();

        let (nodes, _) = get_nodes(&mut rng);

        let pack = make_pack(&tempdir, &nodes);

        let mut test_keys: Vec<Key> = nodes.keys().map(|k| k.clone()).collect();
        let missing_key = Key::new(Box::new([9]), Node::random(&mut rng));
        test_keys.push(missing_key.clone());

        let missing = pack.get_missing(&test_keys[..]).unwrap();
        assert_eq!(vec![missing_key], missing);
    }

    #[test]
    fn test_iter() {
        let mut rng = ChaChaRng::from_seed([0u8; 32]);
        let tempdir = TempDir::new().unwrap();

        let (nodes, _) = get_nodes(&mut rng);

        let pack = make_pack(&tempdir, &nodes);

        let mut keys: Vec<Key> = nodes.keys().map(|k| k.clone()).collect();
        keys.sort_unstable();
        let mut iter_keys = pack.iter().collect::<Result<Vec<Key>>>().unwrap();
        iter_keys.sort_unstable();
        assert_eq!(iter_keys, keys,);
    }

    quickcheck! {
        fn test_file_section_header_serialization(name: Vec<u8>, count: u32) -> bool {
            let header = FileSectionHeader {
                file_name: name.as_ref(),
                count: count,
            };
            let mut buf = vec![];
            header.write(&mut buf).unwrap();
            header == FileSectionHeader::read(&buf).unwrap()
        }

        fn test_history_entry_serialization(
            node: Node,
            p1: Node,
            p2: Node,
            link_node: Node,
            copy_from: Option<Vec<u8>>
        ) -> bool {
            let mut buf = vec![];
            HistoryEntry::write(
                &mut buf,
                &node,
                &p1,
                &p2,
                &link_node,
                &copy_from.as_ref().map(|x| x.as_ref()),
            ).unwrap();
            let entry = HistoryEntry::read(&buf).unwrap();
            assert_eq!(node, entry.node);
            assert_eq!(p1, entry.p1);
            assert_eq!(p2, entry.p2);
            assert_eq!(link_node, entry.link_node);
            true
        }
    }
}
