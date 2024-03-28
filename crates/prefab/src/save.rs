use bevy::{
    ecs::{entity::MapEntities, reflect::ReflectMapEntities},
    prelude::*,
    tasks::IoTaskPool,
    utils::{hashbrown::HashMap, HashSet},
};
use bevy_inspector_egui::egui::{self, Color32, RichText, Stroke};
use space_shared::{EditorPrefabPath, PrefabMarker, PrefabMemoryCache, ext::egui_file};
use std::{any::TypeId, fs::{self, File}, io::Write, path::Path};

use crate::prelude::{EditorRegistry, EditorRegistryExt, SceneAutoChild};

#[derive(Reflect, Default, Component, Clone)]
#[reflect(Component, MapEntities)]
/// Component that holds children entity/prefab information
/// that should be serialized
pub struct ChildrenPrefab(pub Vec<Entity>);

impl ChildrenPrefab {
    pub fn from_children(children: &Children) -> Self {
        Self(children.to_vec())
    }
}

impl MapEntities for ChildrenPrefab {
    #[cfg_attr(tarpaulin, ignore)]
    fn map_entities<M: EntityMapper>(&mut self, entity_mapper: &mut M) {
        self.0 = self
            .0
            .iter()
            .map(|e| entity_mapper.map_entity(*e))
            .collect();
    }
}

struct SaveResourcesPrefabPlugin;

impl Plugin for SaveResourcesPrefabPlugin {
    fn build(&self, app: &mut App) {
        app.editor_registry::<ChildrenPrefab>();

        app.init_resource::<SaveConfig>().init_state::<SaveState>();
    }
}

pub struct SavePrefabPlugin;

impl Plugin for SavePrefabPlugin {
    #[cfg_attr(tarpaulin, ignore)]
    fn build(&self, app: &mut App) {
        app.editor_registry::<ChildrenPrefab>();

        app.init_resource::<SaveConfig>().init_state::<SaveState>();
        
        app.add_event::<PrefabMade>();
        app.init_resource::<PrefabsPath>();
        app.add_plugins(SaveResourcesPrefabPlugin {});

        app.add_systems(
            OnEnter(SaveState::Save),
            (
                prepare_children,
                apply_deferred,
                serialize_scene,
                delete_prepared_children,
            )
                .chain(),
        );

        app.add_systems(Update, (
                prepare_children,
                apply_deferred,
                serialize_prefab,
                delete_prepared_children,
            )
                .chain(),
        );
    }
}

/// This struct determine path to save prefab
#[cfg(not(tarpaulin_include))]
#[derive(Resource, Clone, Default)]
pub struct SaveConfig {
    pub path: Option<EditorPrefabPath>,
}

/// State system using to enable slow logic of saving
#[cfg(not(tarpaulin_include))]
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Hash, States)]
pub enum SaveState {
    Save,
    #[default]
    Idle,
}

#[derive(Resource)]
pub struct PrefabsPath {
    pub path: String,
    pub dialog: Option<egui_file::FileDialog>,
}

impl PrefabsPath {
    pub fn ui(&mut self, ui: &mut egui::Ui, icon_size: f32, stroke_color: Color32) {
        ui.horizontal(|ui| {
            ui.label("Path:");
            ui.spacing();
            let path_button = egui::Button::new(RichText::new("📁").size(icon_size))
                .stroke(Stroke::new(1., stroke_color));
            if ui
                .add(path_button)
                .on_hover_text("Select Prefabs Path")
                .clicked()
            {
                if !Path::new(self.path.as_str()).exists() {
                    std::fs::create_dir_all(self.path.as_str()).unwrap();
                }
                let mut dialog =
                    egui_file::FileDialog::select_folder(Some(self.path.clone().into()))
                        .default_filename("")
                        .title("Prefabs Path");
                dialog.open();
                self.dialog = Some(dialog);
            }
            ui.text_edit_singleline(&mut self.path);
        });
    }
}

impl Default for PrefabsPath {
    fn default() -> Self {
        Self {
            path: ".\\assets\\prefabs".into(),
            dialog: None,
        }
    }
}

#[derive(Event)]
pub struct PrefabMade{
    pub entity: Entity,
}

fn prepare_children(
    mut commands: Commands,
    query: Query<(Entity, &Children), (With<PrefabMarker>, Without<SceneAutoChild>)>,
) {
    for (entity, children) in query.iter() {
        commands
            .entity(entity)
            .insert(ChildrenPrefab::from_children(children));
    }
}

fn delete_prepared_children(mut commands: Commands, query: Query<Entity, With<ChildrenPrefab>>) {
    for entity in query.iter() {
        commands.entity(entity).remove::<ChildrenPrefab>();
    }
}

/// Convert world scene to prefab
pub fn serialize_scene(world: &mut World) {
    let config = world.resource::<SaveConfig>().clone();

    let mut prefab_query =
        world.query_filtered::<Entity, (With<PrefabMarker>, Without<SceneAutoChild>)>();
    let entities = prefab_query.iter(world).collect::<Vec<_>>();

    if entities.is_empty() {
        #[cfg(feature = "editor")]
        world.send_event(space_shared::toast::ToastMessage::new(
            "Saving empty scene",
            space_shared::toast::ToastKind::Warning,
        ));
        warn!("Saving empty scene");
    }

    let registry = world.resource::<EditorRegistry>().clone();
    let allow_types: Vec<TypeId> = registry
        .registry
        .read()
        .iter()
        .map(|a| a.type_id())
        .collect();
    let mut builder = DynamicSceneBuilder::from_world(world);
    builder = builder
        .allow_all()
        .with_filter(SceneFilter::Allowlist(HashSet::from_iter(
            allow_types.iter().cloned(),
        )))
        .extract_entities(entities.iter().copied());
    let scene = builder.build();

    let res = scene.serialize_ron(world.resource::<AppTypeRegistry>());

    if let Ok(str) = res {
        // Write the scene RON data to file
        let path = config.path;
        if let Some(path) = path {
            match path {
                EditorPrefabPath::File(path) => {
                    IoTaskPool::get()
                        .spawn(async move {
                            fs::OpenOptions::new()
                                .create(true)
                                .truncate(true)
                                .append(false)
                                .write(true)
                                .open(&path)
                                .and_then(|mut file| file.write(str.as_bytes()))
                                .inspect_err(|e| error!("Error while writing scene to file: {e}"))
                                .expect("Error while writing scene to file");
                            info!("Saved prefab to file {}", path);
                        })
                        .detach();
                }
                EditorPrefabPath::MemoryCache => {
                    let handle = world.resource_mut::<Assets<DynamicScene>>().add(scene);
                    world.resource_mut::<PrefabMemoryCache>().scene = Some(handle);
                }
            }
        }
    } else if let Err(e) = res {
        // Any ideas on how to test this error case?
        #[cfg_attr(tarpaulin, ignore)]
        let err = format!("failed to serialize prefab: {:?}", e);
        #[cfg(feature = "editor")]
        world.send_event(space_shared::toast::ToastMessage::new(
            &err,
            space_shared::toast::ToastKind::Error,
        ));
        error!(err);
    }

    world
        .resource_mut::<NextState<SaveState>>()
        .set(SaveState::Idle);
}

fn serialize_prefab(
    world: &mut World,
) {
    let mut children_query = world.query_filtered::<(Entity, &Children), With<PrefabMarker>>();
    let prefabs_path = world.resource::<PrefabsPath>();
    let children_map: HashMap<_, _> = children_query.iter(world).collect();
    let events = world.get_resource::<Events<PrefabMade>>().unwrap();
    let mut event_reader = events.get_reader();

    for event in event_reader.read(events){
        let entity = event.entity;
        let mut entities = vec![entity];
        let name_component = world.get::<Name>(entity).unwrap();
        let name = name_component.as_str();
        add_children_recursive(&children_map, entity, &mut entities);
        
        let registry = world.resource::<EditorRegistry>().clone();
        let allow_types: Vec<TypeId> = registry
            .registry
            .read()
            .iter()
            .map(|a| a.type_id())
            .collect();
        let mut builder = DynamicSceneBuilder::from_world(world);
        builder = builder
            .allow_all()
            .with_filter(SceneFilter::Allowlist(HashSet::from_iter(
                allow_types.iter().cloned(),
            )))
            .extract_entities(entities.iter().copied());
        let prefab = builder.build();

        let res = prefab.serialize_ron(world.resource::<AppTypeRegistry>());
        
        if let Ok(str) = res {
            // Write the scene RON data to file
            let path = prefabs_path.path.clone() + "\\" + name + ".scn.ron";
            
            IoTaskPool::get()
                .spawn(async move {
                    File::create(&path)
                        .and_then(|mut file| file.write(str.as_bytes()))
                        .expect("Error while writing prefab to file");
                    info!("Saved prefab to file {}", path);
                })
                .detach();
        } else if let Err(e) = res {
            error!("failed to serialize prefab: {:?}", e);
        }
    }
}

fn add_children_recursive(children_iter: &HashMap<Entity, &Children>, entity: Entity, mut entities: &mut Vec<Entity>) {
    if let Some(children) = children_iter.get(&entity) {
        entities.extend(children.iter().copied());
        for child in children.iter() {
            add_children_recursive(children_iter, *child, &mut entities);
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::prelude::*;

    #[test]
    fn flaky_save_to_file() {
        let file = "test.ron";
        let save_config = SaveConfig {
            path: Some(EditorPrefabPath::File(String::from(file))),
        };
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            ImagePlugin::default(),
            bevy::scene::ScenePlugin,
            EditorRegistryPlugin {},
            SaveResourcesPrefabPlugin {},
        ))
        .insert_resource(save_config)
        .init_resource::<PrefabMemoryCache>()
        .editor_registry::<Name>()
        .editor_registry::<PrefabMarker>()
        .add_systems(Startup, |mut commands: Commands| {
            let child_id = commands.spawn_empty().id();
            commands.spawn(PrefabMarker).add_child(child_id);

            commands.spawn(PrefabMarker).insert(Name::new("my_name"));
        });

        app.update();

        serialize_scene(&mut app.world);

        // Delay for 0.2 second for IOTaskPool to finish
        std::thread::sleep(std::time::Duration::from_secs_f32(0.2));

        assert!(
            std::fs::metadata(format!("./{}", file)).is_ok(),
            "Flaky Test: File not found"
        );

        let contents = std::fs::read_to_string(file).unwrap();

        assert!(contents.contains("my_name"));
        assert!(contents.contains("space_shared::PrefabMarker"));
    }

    #[test]
    fn save_to_memory() {
        let save_config = SaveConfig {
            path: Some(EditorPrefabPath::MemoryCache),
        };
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            ImagePlugin::default(),
            bevy::scene::ScenePlugin,
            EditorRegistryPlugin {},
            SaveResourcesPrefabPlugin {},
        ))
        .insert_resource(save_config)
        .init_resource::<PrefabMemoryCache>()
        .editor_registry::<Name>()
        .editor_registry::<PrefabMarker>()
        .add_systems(Startup, |mut commands: Commands| {
            let child_id = commands.spawn_empty().id();
            commands.spawn(PrefabMarker).add_child(child_id);

            commands.spawn(PrefabMarker).insert(Name::new("name"));
        });

        app.update();

        serialize_scene(&mut app.world);
        assert!(app
            .world
            .resource_mut::<PrefabMemoryCache>()
            .scene
            .is_some());
    }

    #[test]
    fn inserts_prepared_children_component() {
        let mut app = App::new();
        app.add_systems(Startup, |mut commands: Commands| {
            let child_id = commands.spawn_empty().id();
            commands.spawn(PrefabMarker).add_child(child_id);

            commands.spawn(PrefabMarker);
        })
        .add_systems(Update, prepare_children);
        app.update();

        let mut query = app.world.query_filtered::<Entity, With<ChildrenPrefab>>();
        assert_eq!(query.iter(&app.world).count(), 1);
    }

    #[test]
    fn deletes_prepared_children_component() {
        let mut app = App::new();
        app.add_systems(Startup, |mut commands: Commands| {
            let child_id = commands.spawn_empty().id();
            commands
                .spawn(PrefabMarker)
                .insert(ChildrenPrefab(vec![child_id]));
            let child_id = commands.spawn_empty().id();
            commands
                .spawn(PrefabMarker)
                .insert(ChildrenPrefab(vec![child_id]));
            commands.spawn(PrefabMarker);
        })
        .add_systems(Update, delete_prepared_children);
        app.update();

        let mut query = app.world.query_filtered::<Entity, With<ChildrenPrefab>>();
        assert_eq!(query.iter(&app.world).count(), 0);
    }

    #[test]
    fn child_prefab_from_children() {
        let mut world = World::new();
        let child = world.spawn_empty().id();
        world.spawn(PrefabMarker).add_child(child);

        let mut query = world.query::<&Children>();
        let children = query.single(&world);
        let prefab = ChildrenPrefab::from_children(children);

        assert_eq!(prefab.0.len(), 1);
    }

    #[test]
    #[cfg(feature = "editor")]
    fn attempts_to_serialize_empty_scene() {
        let save_config = SaveConfig {
            path: Some(EditorPrefabPath::MemoryCache),
        };
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            ImagePlugin::default(),
            bevy::scene::ScenePlugin,
            EditorRegistryPlugin {},
            SaveResourcesPrefabPlugin {},
        ))
        .add_event::<space_shared::toast::ToastMessage>()
        .insert_resource(save_config)
        .init_resource::<PrefabMemoryCache>();

        app.update();

        serialize_scene(&mut app.world);
        let events = app
            .world
            .resource::<Events<space_shared::toast::ToastMessage>>();

        let mut iter = events.get_reader();
        let iter = iter.read(events);
        iter.for_each(|e| assert_eq!(e.text, "Saving empty scene"));
    }

    #[test]
    fn prepared_children_ignores_scene_auto_child_component() {
        let mut app = App::new();
        app.add_systems(Startup, |mut commands: Commands| {
            let child_id = commands.spawn_empty().id();
            commands
                .spawn((PrefabMarker, SceneAutoChild))
                .add_child(child_id);

            let child_id = commands.spawn_empty().id();
            commands.spawn(PrefabMarker).add_child(child_id);

            commands.spawn(PrefabMarker);
        })
        .add_systems(Update, prepare_children);
        app.update();

        let mut query = app.world.query_filtered::<Entity, With<ChildrenPrefab>>();
        assert_eq!(query.iter(&app.world).count(), 1);
    }
}
