// Copyright 2018 Facebook, Inc.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

use byteorder::WriteBytesExt;
use crypto::digest::Digest;
use crypto::sha1::Sha1;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::iter::FromIterator;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

use ancestors::{AncestorIterator, AncestorTraversal};
use error::Result;
use historyindex::{FileSectionLocation, HistoryIndex, NodeLocation};
use historypack::{FileSectionHeader, HistoryEntry, HistoryPackVersion};
use historystore::{Ancestors, HistoryStore, NodeInfo};
use key::Key;
use packwriter::PackWriter;

#[derive(Debug, Fail)]
#[fail(display = "Mutable History Pack Error: {:?}", _0)]
struct MutableHistoryPackError(String);

pub struct MutableHistoryPack {
    version: HistoryPackVersion,
    dir: PathBuf,
    mem_index: HashMap<Box<[u8]>, HashMap<Key, NodeInfo>>,
}

impl MutableHistoryPack {
    pub fn new(dir: &Path, version: HistoryPackVersion) -> Result<Self> {
        if !dir.is_dir() {
            return Err(MutableHistoryPackError(format!(
                "cannot create mutable historypack in non-directory '{:?}'",
                dir
            ))
            .into());
        }

        Ok(MutableHistoryPack {
            version,
            dir: dir.to_path_buf(),
            mem_index: HashMap::new(),
        })
    }

    pub fn add(&mut self, key: &Key, info: &NodeInfo) -> Result<()> {
        // Ideally we could use something like:
        //     self.mem_index.entry(key.name()).or_insert_with(|| HashMap::new())
        // To get the inner map, then insert our new NodeInfo. Unfortunately it requires
        // key.name().clone() though. So we have to do it the long way to avoid the allocation.
        let entries = self
            .mem_index
            .entry(Box::from(key.name()))
            .or_insert_with(|| HashMap::new());
        entries.insert(key.clone(), info.clone());
        Ok(())
    }

    /// Closes the mutable historypack, returning the path of the final immutable historypack on disk.
    /// The mutable historypack is no longer usable after being closed.
    pub fn close(self) -> Result<PathBuf> {
        let mut data_file = PackWriter::new(NamedTempFile::new_in(&self.dir)?);
        let mut hasher = Sha1::new();

        // Write the header
        let version_u8: u8 = self.version.clone().into();
        data_file.write_u8(version_u8)?;
        hasher.input(&[version_u8]);

        // Store data for the index
        let mut file_sections: Vec<(&Box<[u8]>, FileSectionLocation)> = Default::default();
        let mut nodes: HashMap<&Box<[u8]>, HashMap<Key, NodeLocation>> = Default::default();

        // Write the historypack
        let mut section_buf = Vec::new();
        let mut section_offset = data_file.bytes_written();
        // - In sorted order for deterministic hashes.
        let mut keys = self.mem_index.keys().collect::<Vec<&Box<[u8]>>>();
        keys.sort_unstable();
        for file_name in keys {
            let node_map = self.mem_index.get(file_name).unwrap();
            self.write_section(
                &mut section_buf,
                file_name,
                node_map,
                section_offset as usize,
                &mut nodes,
            )?;
            hasher.input(&section_buf);
            data_file.write_all(&mut section_buf)?;

            let section_location = FileSectionLocation {
                offset: section_offset,
                size: section_buf.len() as u64,
            };
            file_sections.push((file_name, section_location));

            section_offset += section_buf.len() as u64;
            section_buf.clear();
        }

        // Compute the index
        let mut index_file = PackWriter::new(NamedTempFile::new_in(&self.dir)?);
        HistoryIndex::write(&mut index_file, &file_sections, &nodes)?;

        // Persist the temp files
        let base_filepath = self.dir.join(&hasher.result_str());
        let data_filepath = base_filepath.with_extension("histpack");
        let index_filepath = base_filepath.with_extension("histidx");

        let data_file = data_file.into_inner()?;

        let mut perms = data_file.as_file().metadata()?.permissions();
        perms.set_readonly(true);

        data_file.as_file().set_permissions(perms.clone())?;

        let index_file = index_file.into_inner()?;
        index_file.as_file().set_permissions(perms)?;

        data_file.persist(&data_filepath)?;
        index_file.persist(&index_filepath)?;
        Ok(base_filepath)
    }

    fn write_section<'a>(
        &self,
        writer: &mut Vec<u8>,
        file_name: &'a Box<[u8]>,
        node_map: &HashMap<Key, NodeInfo>,
        section_offset: usize,
        nodes: &mut HashMap<&'a Box<[u8]>, HashMap<Key, NodeLocation>>,
    ) -> Result<()> {
        let mut node_locations = HashMap::<Key, NodeLocation>::with_capacity(node_map.len());

        // Write section header
        FileSectionHeader {
            file_name: &file_name,
            count: node_map.len() as u32,
        }.write(writer)?;

        // Sort the nodes in topological order (ancestors first), as required by the histpack spec
        let node_map = topo_sort(node_map)?;

        // Write nodes
        for (key, node_info) in node_map.iter() {
            let p1 = &node_info.parents[0];
            let copyfrom = if !p1.node().is_null() && p1.name() != key.name() {
                Some(p1.name())
            } else {
                None
            };

            let node_offset = section_offset + writer.len() as usize;
            HistoryEntry::write(
                writer,
                key.node(),
                node_info.parents[0].node(),
                node_info.parents[1].node(),
                &node_info.linknode,
                &copyfrom,
            )?;

            node_locations.insert(
                (*key).clone(),
                NodeLocation {
                    offset: node_offset as u64,
                },
            );
        }

        nodes.insert(file_name, node_locations);
        Ok(())
    }
}

fn topo_sort(node_map: &HashMap<Key, NodeInfo>) -> Result<Vec<(&Key, &NodeInfo)>> {
    // Sorts the given keys into newest-first topological order
    let mut roots = Vec::<&Key>::new();

    // Child map will be used to perform an oldest-first walk later.
    let mut child_map = HashMap::<&Key, HashSet<&Key>>::with_capacity(node_map.len());
    // Parent count will be used to keep track of when all a commit's parents have been processed.
    let mut parent_counts = HashMap::with_capacity(node_map.len());

    for (key, info) in node_map.iter() {
        let mut parent_count = 0;
        for i in 0..2 {
            let parent = &info.parents[i];

            // Only record the relationship if the parent is also in the provided node_map.
            // This also filters out null parents.
            if node_map.contains_key(parent) {
                parent_count += 1;
                let children = child_map.entry(parent).or_default();
                children.insert(key);
            }
        }

        if parent_count == 0 {
            roots.push(key);
        } else {
            parent_counts.insert(key, parent_count);
        }
    }

    // Sort the roots so things are deterministic.
    roots.sort_unstable();

    // Process roots, adding children to the queue once all their parents are processed.
    let mut pending = VecDeque::<&Key>::from_iter(roots.iter().cloned());
    let mut results = Vec::new();
    while let Some(key) = pending.pop_front() {
        results.push((key, node_map.get(key).unwrap()));

        if let Some(children) = child_map.get(key) {
            for child in children.iter() {
                let mut parent_count = parent_counts
                    .get(child)
                    .ok_or_else(|| {
                        MutableHistoryPackError(format!("missing {:?} during topo sort", child))
                    })?
                    .clone();
                parent_count -= 1;
                parent_counts.insert(child, parent_count);
                if parent_count == 0 {
                    // If a child has no more parents, its a root and is ready for processing.
                    // Put it at the front so ancestor chains are processed contiguously.
                    pending.push_front(child);
                }
            }
        }
    }

    // We built the result in oldest first order, but we need it in newest first order.
    results.reverse();

    assert_eq!(results.len(), node_map.len());
    Ok(results)
}

impl HistoryStore for MutableHistoryPack {
    fn get_ancestors(&self, key: &Key) -> Result<Ancestors> {
        AncestorIterator::new(
            key,
            |k, _seen| self.get_node_info(k),
            AncestorTraversal::Partial,
        ).collect()
    }

    fn get_node_info(&self, key: &Key) -> Result<NodeInfo> {
        Ok(self.mem_index
            .get(key.name())
            .ok_or(MutableHistoryPackError(format!(
                "key '{:?}' not present in mutable history pack",
                key
            )))?
            .get(key)
            .ok_or(MutableHistoryPackError(format!(
                "key '{:?}' not present in mutable history pack",
                key
            )))?
            .clone())
    }

    fn get_missing(&self, keys: &[Key]) -> Result<Vec<Key>> {
        Ok(keys.iter()
            .filter(|k| match self.mem_index.get(k.name()) {
                Some(e) => e.get(k).is_none(),
                None => true,
            })
            .map(|k| k.clone())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::Rng;
    use rand::SeedableRng;
    use rand_chacha::ChaChaRng;
    use tempfile::tempdir;

    use historypack::HistoryPack;
    use repack::IterableStore;
    use types::node::Node;

    #[test]
    fn test_topo_order() {
        // Tests for exponential time complexity in a merge ancestory. This doesn't won't fail,
        // but may take a long time if there is bad time complexity.
        let mut rng = ChaChaRng::from_seed([0u8; 32]);
        let tempdir = tempdir().unwrap();
        let mut muthistorypack =
            MutableHistoryPack::new(tempdir.path(), HistoryPackVersion::One).unwrap();
        let null_key = Key::new(Box::from([]), Node::null_id().clone());

        let chain_count = 2;
        let chain_len = 3;

        let mut chains = HashMap::<Key, Vec<(Key, NodeInfo)>>::new();
        let mut entries = Vec::<(Key, NodeInfo)>::new();
        for _ in 0..chain_count {
            let mut chain = Vec::<(Key, NodeInfo)>::new();
            for i in 0..chain_len {
                let p1 = if i > 0 {
                    chain[i - 1].0.clone()
                } else {
                    null_key.clone()
                };
                let p2 = if i > 1 {
                    chain[i - 2].0.clone()
                } else {
                    null_key.clone()
                };

                let key = Key::new(Box::from([]), Node::random(&mut rng));
                let info = NodeInfo {
                    parents: [p1, p2],
                    linknode: Node::random(&mut rng),
                };
                entries.push((key.clone(), info.clone()));
                chain.push((key.clone(), info.clone()));
                if i == chain_len - 1 {
                    // Reverse it so the newest key is first.
                    chain.reverse();
                    chains.insert(key, chain.clone());
                }
            }
        }

        // Add them in random order, so we can verify they get sorted correctly
        rng.shuffle(&mut entries);
        for (key, info) in entries.iter() {
            muthistorypack.add(&key, &info).unwrap();
        }
        let path = muthistorypack.close().unwrap();
        let pack = HistoryPack::new(&path).unwrap();

        let actual_order = pack.iter().map(|x| x.unwrap()).collect::<Vec<Key>>();

        // Compute the expected order
        let mut chains = chains.iter().collect::<Vec<_>>();
        chains.sort_unstable();
        chains.reverse();
        let mut expected_order = vec![];
        for (_, chain) in chains.iter() {
            for (key, _) in chain.iter() {
                expected_order.push(key.clone());
            }
        }

        assert_eq!(actual_order, expected_order);
    }

    quickcheck! {
        fn test_get_ancestors(keys: Vec<(Key, bool)>) -> bool {
            let mut rng = ChaChaRng::from_seed([0u8; 32]);
            let tempdir = tempdir().unwrap();
            let mut muthistorypack =
                MutableHistoryPack::new(tempdir.path(), HistoryPackVersion::One).unwrap();

            // Insert all the keys, randomly choosing nodes from the already inserted keys
            let mut chains = HashMap::<Key, Ancestors>::new();
            chains.insert(Key::default(), Ancestors::new());
            for &(ref key, ref has_p2) in keys.iter() {
                let mut p1 = Key::default();
                let mut p2 = Key::default();
                let available_parents = chains.keys().map(|k| k.clone()).collect::<Vec<Key>>();

                if !chains.is_empty() {
                    p1 = rng.choose(&available_parents[..])
                        .expect("choose p1")
                        .clone();

                    if *has_p2 {
                        p2 = rng.choose(&available_parents[..])
                            .expect("choose p2")
                            .clone();
                    }
                }

                // Insert into the history pack
                let info = NodeInfo {
                    parents: [p1.clone(), p2.clone()],
                    linknode: Node::random(&mut rng),
                };
                muthistorypack.add(&key, &info).unwrap();

                // Compute the ancestors for the inserted key
                let p1_ancestors = chains.get(&p1).expect("get p1 ancestors").clone();
                let p2_ancestors = chains.get(&p2).expect("get p2 ancestors").clone();
                let mut ancestors = Ancestors::new();
                ancestors.extend(p1_ancestors);
                ancestors.extend(p2_ancestors);
                ancestors.insert(key.clone(), info.clone());
                chains.insert(key.clone(), ancestors);
            }

            for &(ref key, _) in keys.iter() {
                let in_pack = muthistorypack.get_ancestors(&key).expect("get ancestors");
                if in_pack != chains[&key] {
                    return false;
                }
            }

            true
        }

        fn test_get_node_info(insert: HashMap<Key, NodeInfo>, notinsert: Vec<Key>) -> bool {
            let tempdir = tempdir().unwrap();
            let mut muthistorypack =
                MutableHistoryPack::new(tempdir.path(), HistoryPackVersion::One).unwrap();

            for (key, info) in insert.iter() {
                muthistorypack.add(&key, &info).unwrap();
            }

            for (key, info) in insert.iter() {
                if *info != muthistorypack.get_node_info(key).unwrap() {
                    return false;
                }
            }

            for key in notinsert.iter() {
                if muthistorypack.get_node_info(key).is_ok() {
                    return false;
                }
            }

            true
        }

        fn test_get_missing(insert: HashMap<Key, NodeInfo>, notinsert: Vec<Key>) -> bool {
            let tempdir = tempdir().unwrap();
            let mut muthistorypack =
                MutableHistoryPack::new(tempdir.path(), HistoryPackVersion::One).unwrap();

            for (key, info) in insert.iter() {
                muthistorypack.add(&key, &info).unwrap();
            }

            let mut lookup = notinsert.clone();
            lookup.extend(insert.keys().map(|k| k.clone()));

            let missing = muthistorypack.get_missing(&lookup).unwrap();
            missing == notinsert
        }
    }
}
