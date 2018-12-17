use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex},
    str,
};

use rbx_tree::{RbxTree, RbxValue, RbxId};

use crate::{
    project::{Project, ProjectNode, InstanceProjectNode},
    message_queue::MessageQueue,
    imfs::{Imfs, ImfsItem, ImfsFile},
    path_map::PathMap,
    rbx_snapshot::{RbxSnapshotInstance, InstanceChanges, reify_root, reconcile_subtree},
};

pub struct RbxSession {
    tree: RbxTree,
    paths_to_node_ids: PathMap<RbxId>,
    ids_to_project_paths: HashMap<RbxId, String>,
    message_queue: Arc<MessageQueue<InstanceChanges>>,
    imfs: Arc<Mutex<Imfs>>,
    project: Arc<Project>,
}

impl RbxSession {
    pub fn new(
        project: Arc<Project>,
        imfs: Arc<Mutex<Imfs>>,
        message_queue: Arc<MessageQueue<InstanceChanges>>,
    ) -> RbxSession {
        let tree = {
            let temp_imfs = imfs.lock().unwrap();
            construct_initial_tree(&project, &temp_imfs)
        };

        // TODO: Restore these?
        let paths_to_node_ids = PathMap::new();
        let ids_to_project_paths = HashMap::new();

        RbxSession {
            tree,
            paths_to_node_ids,
            ids_to_project_paths,
            message_queue,
            imfs,
            project,
        }
    }

    fn path_created_or_updated(&mut self, path: &Path) {
        let imfs = self.imfs.lock().unwrap();
        let root_path = imfs.get_root_for_path(path)
            .expect("Path was outside in-memory filesystem roots");

        let closest_path = self.paths_to_node_ids.descend(root_path, path);
        let &instance_id = self.paths_to_node_ids.get(&closest_path).unwrap();

        let snapshot = snapshot_instances_from_imfs(&imfs, &closest_path)
            .expect("Could not generate instance snapshot");

        let mut changes = InstanceChanges::default();
        reconcile_subtree(&mut self.tree, instance_id, &snapshot, &mut changes);
    }

    pub fn path_created(&mut self, path: &Path) {
        info!("Path created: {}", path.display());
        self.path_created_or_updated(path);
    }

    pub fn path_updated(&mut self, path: &Path) {
        info!("Path updated: {}", path.display());

        {
            let imfs = self.imfs.lock().unwrap();

            // If the path doesn't exist or it's a directory, we don't care if it
            // updated
            match imfs.get(path) {
                Some(ImfsItem::Directory(_)) | None => return,
                Some(ImfsItem::File(_)) => {},
            }
        }

        self.path_created_or_updated(path);
    }

    pub fn path_removed(&mut self, path: &Path) {
        info!("Path removed: {}", path.display());

        let instance_id = match self.paths_to_node_ids.remove(path) {
            Some(id) => id,
            None => return,
        };

        let removed_subtree = match self.tree.remove_instance(instance_id) {
            Some(tree) => tree,
            None => {
                warn!("Rojo tried to remove an instance that was half cleaned-up. This is probably a bug in Rojo.");
                return;
            },
        };

        let changes = InstanceChanges {
            added: HashSet::new(),
            removed: removed_subtree.iter_all_ids().collect(),
            updated: HashSet::new(),
        };

        self.message_queue.push_messages(&[changes]);
    }

    pub fn path_renamed(&mut self, from_path: &Path, to_path: &Path) {
        info!("Path renamed from {} to {}", from_path.display(), to_path.display());
        self.path_removed(from_path);
        self.path_created(to_path);
    }

    pub fn get_tree(&self) -> &RbxTree {
        &self.tree
    }

    pub fn get_project_path_map(&self) -> &HashMap<RbxId, String> {
        &self.ids_to_project_paths
    }
}

pub fn construct_oneoff_tree(project: &Project, imfs: &Imfs) -> RbxTree {
    construct_initial_tree(project, imfs)
}

fn construct_initial_tree(
    project: &Project,
    imfs: &Imfs,
) -> RbxTree {
    let snapshot = construct_project_node(
        imfs,
        &project.name,
        &project.tree,
    );

    let mut changes = InstanceChanges::default();

    reify_root(&snapshot, &mut changes)
}

fn construct_project_node<'a>(
    imfs: &'a Imfs,
    instance_name: &'a str,
    project_node: &'a ProjectNode,
) -> RbxSnapshotInstance<'a> {
    match project_node {
        ProjectNode::Instance(node) => {
            construct_instance_node(imfs, instance_name, node)
        },
        ProjectNode::SyncPoint(node) => {
            snapshot_instances_from_imfs(imfs, &node.path)
                .expect("Could not reify nodes from Imfs")
        },
    }
}

fn construct_instance_node<'a>(
    imfs: &'a Imfs,
    instance_name: &'a str,
    project_node: &'a InstanceProjectNode,
) -> RbxSnapshotInstance<'a> {
    let mut children = Vec::new();

    for (child_name, child_project_node) in &project_node.children {
        children.push(construct_project_node(imfs, child_name, child_project_node));
    }

    RbxSnapshotInstance {
        class_name: Cow::Borrowed(&project_node.class_name),
        name: Cow::Borrowed(instance_name),
        properties: HashMap::new(),
        children,
        source_path: None,
    }
}

#[derive(Debug, Clone, Copy)]
enum FileType {
    ModuleScript,
    ServerScript,
    ClientScript,
}

fn get_trailing<'a>(input: &'a str, trailer: &str) -> Option<&'a str> {
    if input.ends_with(trailer) {
        let end = input.len().saturating_sub(trailer.len());
        Some(&input[..end])
    } else {
        None
    }
}

fn classify_file(file: &ImfsFile) -> Option<(&str, FileType)> {
    let file_name = file.path.file_name()?.to_str()?;

    if let Some(instance_name) = get_trailing(file_name, ".server.lua") {
        Some((instance_name, FileType::ServerScript))
    } else if let Some(instance_name) = get_trailing(file_name, ".client.lua") {
        Some((instance_name, FileType::ClientScript))
    } else if let Some(instance_name) = get_trailing(file_name, ".lua") {
        Some((instance_name, FileType::ModuleScript))
    } else {
        None
    }
}

fn snapshot_instances_from_imfs<'a>(imfs: &'a Imfs, imfs_path: &Path) -> Option<RbxSnapshotInstance<'a>> {
    match imfs.get(imfs_path)? {
        ImfsItem::File(file) => {
            let (instance_name, file_type) = classify_file(file)?;

            let class_name = match file_type {
                FileType::ModuleScript => "ModuleScript",
                FileType::ServerScript => "Script",
                FileType::ClientScript => "LocalScript",
            };

            let contents = str::from_utf8(&file.contents)
                .expect("File did not contain UTF-8 data, which is required for scripts.");

            let mut properties = HashMap::new();
            properties.insert(String::from("Source"), RbxValue::String {
                value: contents.to_string(),
            });

            Some(RbxSnapshotInstance {
                name: Cow::Borrowed(instance_name),
                class_name: Cow::Borrowed(class_name),
                properties,
                children: Vec::new(),
                source_path: Some(file.path.clone()),
            })
        },
        ImfsItem::Directory(directory) => {
            let init_path = directory.path.join("init.lua");

            let mut instance = if directory.children.contains(&init_path) {
                snapshot_instances_from_imfs(imfs, &init_path)?
            } else {
                RbxSnapshotInstance {
                    class_name: Cow::Borrowed("Folder"),
                    name: Cow::Borrowed(""), // Assigned later in the method
                    properties: HashMap::new(),
                    children: Vec::new(),
                    source_path: Some(directory.path.clone()),
                }
            };

            instance.name = Cow::Borrowed(directory.path.file_name()?.to_str()?);

            for child_path in &directory.children {
                if child_path != &init_path {
                    instance.children.push(snapshot_instances_from_imfs(imfs, child_path)?);
                }
            }

            Some(instance)
        },
    }
}