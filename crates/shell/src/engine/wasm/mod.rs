//! The WebAssembly engine: applications are core WebAssembly modules (see
//! `gpui-shell-wasm-abi`), and no JavaScript engine is linked.
//!
//! It provides the same `ShellRuntime` surface the QuickJS engine does, so
//! everything above the seam (the snapshot lifecycle, materialization, the
//! plugin manifest and policy, hot reload, retained entities, callback
//! generations, overlays, docks) runs unchanged. What differs is what a
//! script value is:
//!
//! * A view object is a guest instance and the part of it a view shows.
//! * A callback is the guest's handler number; dispatching one hands the
//!   guest a JSON payload in the shape a script handler's first argument has.
//! * A render records the guest's postcard description into the spec arena
//!   ([`guest`] does the recording).
//!
//! Work a guest starts (a background call, a stream) is a GPUI task owned by
//! the view, application generation and policy it was started under, and is
//! cancelled with them, as a script's tasks are.

mod guest;

use std::{
    cell::{Cell, RefCell, RefMut},
    collections::HashMap,
    path::Path,
    rc::{Rc, Weak},
};

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, AppContext as _, ClickEvent, Entity, EntityId, Global, WeakEntity, Window};
use serde_json::{Value as Json, json};

use super::component_host::{self, AppEffectInstall};
use crate::{
    ComponentCallbackArgument, ComponentCallbackValue, ComponentDataValue, FrozenComponentRegistry,
    entities::{EntityHandle, EntityStore},
    host_modules::HostValue,
    metrics::Metrics,
    policy::Policy,
    runtime::{ApplicationGeneration, CallbackArena, CallbackEntry},
    scope::{self, ScopePhase},
    snapshot::RenderSnapshot,
    spec::{CallbackId, SpecArena},
    view::ScriptView,
};

pub(crate) use guest::Guest;

/// A compiled application module, ready to instantiate.
#[derive(Clone)]
pub struct ViewType {
    module: wasmtime::Module,
    application: Rc<ApplicationGeneration>,
}

/// An application loaded by [`ShellRuntime::load_application`], not yet
/// mounted.
pub struct LoadedApplication {
    runtime: Weak<ShellRuntime>,
    view_type: ViewType,
    mounted: Cell<bool>,
}

/// What a view shows: one guest instance, and which of its parts.
#[derive(Clone)]
pub struct ViewObject {
    guest: Rc<Guest>,
    part: u32,
    /// Whether this view owns the guest's application generation. Only the
    /// root does: a part's view (a dialog, a panel, a child view) going away
    /// must not retire the application.
    owns_application: bool,
}

impl std::fmt::Debug for ViewObject {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ViewObject")
            .field("part", &self.part)
            .finish_non_exhaustive()
    }
}

impl ViewObject {
    fn root(guest: Rc<Guest>) -> Self {
        Self {
            guest,
            part: 0,
            owns_application: true,
        }
    }

    fn part(guest: Rc<Guest>, part: u32) -> Self {
        Self {
            guest,
            part,
            owns_application: false,
        }
    }

    pub(crate) fn application_generation(&self) -> Option<Rc<ApplicationGeneration>> {
        self.owns_application
            .then(|| self.guest.application())
            .flatten()
    }
}

/// A guest handler in the callback arena: which guest, and its number there.
#[derive(Clone)]
pub(crate) struct GuestHandler {
    guest: Weak<Guest>,
    handler: u32,
}

/// The App can find its default runtime without becoming its owner.
struct RuntimeGlobal(Weak<ShellRuntime>);

impl Global for RuntimeGlobal {}

pub struct ShellRuntime {
    callbacks: RefCell<CallbackArena<GuestHandler>>,
    components: FrozenComponentRegistry,
    component_states: RefCell<crate::component_registry::RetainedStateStore>,
    pending_component_state_releases: RefCell<Vec<Rc<ApplicationGeneration>>>,
    component_app_effects: component_host::AppEffects,
    arena: RefCell<SpecArena>,
    entities: RefCell<EntityStore>,
    /// Every live guest, so an application generation's release reaches the
    /// processes and sockets its guests started.
    guests: RefCell<Vec<Rc<Guest>>>,
    metrics: Metrics,
    engine: wasmtime::Engine,
    next_application_generation: Cell<u64>,
}

impl Drop for ShellRuntime {
    fn drop(&mut self) {
        component_host::flush_state_releases(
            &self.component_states,
            &self.pending_component_state_releases,
        );
        self.callbacks.borrow_mut().clear();
        for guest in self.guests.borrow_mut().drain(..) {
            guest.stop_resources();
        }
        self.entities.borrow_mut().clear();
    }
}

impl ShellRuntime {
    /// Loads an application's module without exposing engine values.
    pub fn load_application(
        self: &Rc<Self>,
        directory: &Path,
        entry: &str,
    ) -> Result<LoadedApplication> {
        Ok(LoadedApplication {
            runtime: Rc::downgrade(self),
            view_type: self.load_app(directory, entry)?,
            mounted: Cell::new(false),
        })
    }

    /// Instantiates and mounts a loaded application as a [`ScriptView`].
    pub fn mount_application(
        self: &Rc<Self>,
        application: &LoadedApplication,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<ScriptView>> {
        anyhow::ensure!(
            application
                .runtime
                .upgrade()
                .is_some_and(|runtime| Rc::ptr_eq(&runtime, self)),
            "loaded application belongs to a different ShellRuntime"
        );
        anyhow::ensure!(
            !application.mounted.replace(true),
            "loaded application has already been mounted"
        );
        self.instantiate_view_with_policy(
            &application.view_type,
            crate::policy::default(),
            window,
            cx,
        )
    }

    /// Creates the application's default runtime and makes it available to
    /// shell callbacks registered on this [`App`].
    pub fn new(cx: &mut App) -> Result<Rc<Self>> {
        Self::new_with_components(cx, FrozenComponentRegistry::default())
    }

    pub fn new_with_components(
        cx: &mut App,
        components: FrozenComponentRegistry,
    ) -> Result<Rc<Self>> {
        if Self::global(cx).is_some() {
            return Err(anyhow!(
                "a default gpui-shell runtime is already installed; use ShellRuntime::new_isolated() for an additional runtime"
            ));
        }
        let runtime = Self::new_isolated_with_components(components)?;
        runtime.set_global(cx);
        Ok(runtime)
    }

    /// Creates a runtime without installing it as the application's default.
    pub fn new_isolated() -> Result<Rc<Self>> {
        Self::new_isolated_with_components(FrozenComponentRegistry::default())
    }

    pub fn new_isolated_with_components(components: FrozenComponentRegistry) -> Result<Rc<Self>> {
        let entities = EntityStore::try_new()
            .ok_or_else(|| anyhow!("gpui-shell entity store id space is exhausted"))?;
        Ok(Rc::new(Self {
            callbacks: RefCell::new(CallbackArena::default()),
            components,
            component_states: RefCell::new(Default::default()),
            pending_component_state_releases: RefCell::new(Vec::new()),
            component_app_effects: RefCell::new(HashMap::new()),
            arena: RefCell::new(SpecArena::new()),
            entities: RefCell::new(entities),
            guests: RefCell::new(Vec::new()),
            metrics: Metrics::default(),
            engine: wasmtime::Engine::default(),
            next_application_generation: Cell::new(1),
        }))
    }

    pub fn component_registry(&self) -> &FrozenComponentRegistry {
        &self.components
    }

    pub(crate) fn component_entity_kind(&self, handle: EntityHandle) -> Option<&'static str> {
        self.entities.borrow().kind(handle)
    }

    pub(crate) fn with_component_state<T: std::any::Any, R>(
        &self,
        handle: u64,
        kind: &'static str,
        body: impl FnOnce(&T) -> R,
    ) -> Result<R> {
        component_host::with_state(
            &self.component_states,
            &self.pending_component_state_releases,
            handle,
            kind,
            body,
        )
    }

    pub(crate) fn update_component_state<T: std::any::Any, R>(
        self: &Rc<Self>,
        handle: u64,
        kind: &'static str,
        window: &mut Window,
        cx: &mut App,
        body: impl FnOnce(&mut T, &mut Window, &mut App) -> R,
    ) -> Result<R> {
        anyhow::ensure!(
            !matches!(
                scope::current_phase(),
                Some(ScopePhase::Render | ScopePhase::Layout)
            ),
            "retained component state cannot be updated during render or layout"
        );
        let (_scope, _) = scope::enter_runtime(self, window, cx, ScopePhase::Event, None);
        component_host::update_state(
            &self.component_states,
            &self.pending_component_state_releases,
            handle,
            kind,
            window,
            cx,
            body,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn schedule_component_app_effect(
        self: &Rc<Self>,
        application: Rc<ApplicationGeneration>,
        view: WeakEntity<ScriptView>,
        key: String,
        revision: String,
        window: &mut Window,
        cx: &mut App,
        install: AppEffectInstall,
    ) -> Result<()> {
        component_host::schedule_app_effect(
            self,
            application,
            view,
            key,
            revision,
            window,
            cx,
            install,
        )
    }

    pub(crate) fn component_app_effects(&self) -> &component_host::AppEffects {
        &self.component_app_effects
    }

    fn flush_component_state_releases(&self) {
        component_host::flush_state_releases(
            &self.component_states,
            &self.pending_component_state_releases,
        );
    }

    #[cfg(test)]
    pub(crate) fn retained_component_state_count(&self) -> usize {
        self.component_states
            .try_borrow()
            .map_or(0, |states| states.len())
    }

    /// TypeScript declarations are what a script's editor reads; a guest is
    /// typed by its own SDK, so this engine has none to give.
    pub fn type_declarations(&self) -> String {
        String::new()
    }

    pub fn write_type_declarations(
        &self,
        _root: &Path,
    ) -> std::io::Result<Vec<std::path::PathBuf>> {
        Ok(Vec::new())
    }

    /// A script engine writes editor files into an application on load; a
    /// guest is built by its own toolchain, so there is nothing to switch.
    pub fn set_editor_files(&self, _enabled: bool) {}

    pub(crate) fn set_global(self: &Rc<Self>, cx: &mut App) {
        cx.set_global(RuntimeGlobal(Rc::downgrade(self)));
    }

    pub(crate) fn global(cx: &App) -> Option<Rc<Self>> {
        cx.try_global::<RuntimeGlobal>()
            .and_then(|global| global.0.upgrade())
    }

    pub(crate) fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    pub fn read_metrics(&self) -> crate::metrics::RuntimeMetrics {
        self.metrics.read()
    }

    pub(crate) fn entities(&self) -> RefMut<'_, EntityStore> {
        self.entities.borrow_mut()
    }

    /// The description being recorded.
    pub(crate) fn arena_mut(&self) -> RefMut<'_, SpecArena> {
        self.arena.borrow_mut()
    }

    pub(crate) fn release_view_handle(
        &self,
        handle: EntityHandle,
        cx: &mut impl gpui::AppContext,
    ) -> bool {
        let release = { self.entities().release_view(handle) };
        let Some(release) = release else {
            return false;
        };
        release.retire(cx);
        true
    }

    pub(crate) fn release_application_generation(
        &self,
        application: &Rc<ApplicationGeneration>,
        cx: &mut impl gpui::AppContext,
    ) {
        component_host::retire_app_effects(&self.component_app_effects, application, cx);
        self.release_component_states(application);
        let release = { self.entities().release_application(application) };
        release.retire(cx);
        cancel_application_tasks(application);
        self.release_guests(application);
    }

    pub(crate) fn release_application_generation_without_context(
        &self,
        application: &Rc<ApplicationGeneration>,
    ) {
        self.release_component_states(application);
        let release = { self.entities().release_application(application) };
        release.retire_without_context();
        cancel_application_tasks(application);
        self.release_guests(application);
    }

    fn release_component_states(&self, application: &Rc<ApplicationGeneration>) {
        self.pending_component_state_releases
            .borrow_mut()
            .push(application.clone());
        self.flush_component_state_releases();
    }

    /// Stops what `application`'s guests started and forgets them. Taken out
    /// first and stopped after the borrow ends: stopping drops host closures.
    fn release_guests(&self, application: &Rc<ApplicationGeneration>) {
        application.retire();
        let released: Vec<_> = {
            let mut guests = self.guests.borrow_mut();
            let (released, kept) = guests.drain(..).partition(|guest: &Rc<Guest>| {
                guest
                    .application()
                    .is_some_and(|owner| Rc::ptr_eq(&owner, application))
            });
            *guests = kept;
            released
        };
        for guest in released {
            guest.stop_resources();
        }
    }

    #[cfg(test)]
    pub(crate) fn live_guests(&self) -> usize {
        self.guests.borrow().len()
    }

    /// Compiles an application's module. `entry` names it inside the
    /// application directory, as a manifest's `entry` does.
    pub(crate) fn load_app(self: &Rc<Self>, dir: &Path, entry: &str) -> Result<ViewType> {
        let root = crate::runtime::resolve_app_root(dir, entry)?;
        let path = root.join(entry);
        anyhow::ensure!(
            path.extension()
                .is_some_and(|extension| extension == "wasm"),
            "{} is not a WebAssembly module; this runtime runs `.wasm` entries only",
            path.display()
        );
        let bytes =
            std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
        let module = wasmtime::Module::new(&self.engine, &bytes)
            .with_context(|| format!("{} is not a WebAssembly module", path.display()))?;
        let application = ApplicationGeneration::new(self.next_application_generation.get());
        self.next_application_generation.set(
            self.next_application_generation
                .get()
                .checked_add(1)
                .expect("a shell runtime exhausted its application generations"),
        );
        Ok(ViewType {
            module,
            application,
        })
    }

    /// Constructs and initializes a guest under its final view, as a script
    /// view's `init` runs once its entity exists: work `init` starts then has
    /// an owner, and a later notify can reach the view.
    pub(crate) fn instantiate_view_with_policy(
        self: &Rc<Self>,
        view_type: &ViewType,
        policy: Rc<Policy>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<ScriptView>> {
        let application = Some(view_type.application.clone());
        let constructed = {
            let (_scope, _) = scope::enter_with_application(
                self,
                window,
                cx,
                ScopePhase::Event,
                None,
                policy.clone(),
                application.clone(),
            );
            Guest::new(self, &view_type.module, view_type.application.clone())
        };
        let guest = match constructed {
            Ok(guest) => guest,
            Err(error) => {
                self.release_application_generation(&view_type.application, cx);
                return Err(error);
            }
        };
        let object = ViewObject::root(guest.clone());
        let view = cx.new(|_| ScriptView::with_policy(self.clone(), object, policy.clone()));
        let (_scope, _) = scope::enter_with_application(
            self,
            window,
            cx,
            ScopePhase::Event,
            Some(view.clone()),
            policy,
            application,
        );
        if let Err(error) = guest::initialize(self, &guest, Some(&view)) {
            self.release_application_generation(&view_type.application, cx);
            return Err(error);
        }
        Ok(view)
    }

    /// A guest drawn by no view, for a test that builds its snapshots itself.
    #[cfg(test)]
    pub(crate) fn instantiate(
        self: &Rc<Self>,
        view_type: &ViewType,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<ViewObject> {
        let (_scope, _) = scope::enter_with_application(
            self,
            window,
            cx,
            ScopePhase::Event,
            None,
            crate::policy::default(),
            Some(view_type.application.clone()),
        );
        let guest = Guest::new(self, &view_type.module, view_type.application.clone())?;
        guest::initialize(self, &guest, None)?;
        Ok(ViewObject::root(guest))
    }

    #[cfg(test)]
    pub(crate) fn instantiate_view(
        self: &Rc<Self>,
        view_type: &ViewType,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<ScriptView>> {
        self.instantiate_view_with_policy(view_type, crate::policy::default(), window, cx)
    }

    /// A fresh guest for a view that already exists: what a reload swaps in.
    pub(crate) fn instantiate_for_view(
        self: &Rc<Self>,
        view_type: &ViewType,
        view: Entity<ScriptView>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<ViewObject> {
        let policy = view.read(cx).policy();
        let (_scope, _) = scope::enter_with_application(
            self,
            window,
            cx,
            ScopePhase::Event,
            Some(view.clone()),
            policy,
            Some(view_type.application.clone()),
        );
        let initialized = Guest::new(self, &view_type.module, view_type.application.clone())
            .and_then(|guest| guest::initialize(self, &guest, Some(&view)).map(|()| guest));
        match initialized {
            Ok(guest) => Ok(ViewObject::root(guest)),
            Err(error) => {
                self.release_application_generation(&view_type.application, cx);
                Err(error)
            }
        }
    }

    /// A view showing one of a guest's parts.
    fn part_view(
        self: &Rc<Self>,
        guest: &Rc<Guest>,
        part: u32,
        nested_in: Option<EntityId>,
        cx: &mut App,
    ) -> Entity<ScriptView> {
        let object = ViewObject::part(guest.clone(), part);
        let policy = scope::policy();
        let runtime = self.clone();
        let view = cx.new(|_| match nested_in {
            Some(parent) => ScriptView::nested(runtime, object, policy, parent),
            None => ScriptView::with_policy(runtime, object, policy),
        });
        guest.add_view(&view);
        view
    }

    /// Runs the guest's render for one view and publishes what it described.
    ///
    /// Transactional, as a script render is: the scratch arena and an open
    /// callback generation are staging, published together at the end, and a
    /// render that fails discards both.
    pub(crate) fn build_snapshot(
        self: &Rc<Self>,
        object: &ViewObject,
        view: Option<Entity<ScriptView>>,
        policy: Rc<Policy>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<RenderSnapshot> {
        crate::theme_tokens::sync(cx);
        self.arena.borrow_mut().reset();
        let callbacks = self.callbacks.borrow_mut().begin();
        let application = object.guest.application();

        let root = self.metrics.time_script_render(|| {
            let (_guard, _) = scope::enter_with_application(
                self,
                window,
                cx,
                ScopePhase::Render,
                view.clone(),
                policy,
                application.clone(),
            );
            object
                .guest
                .render(object.part)
                .and_then(|description| guest::record(self, &object.guest, &description))
        });
        let root = match root {
            Ok(root) => root,
            Err(error) => {
                self.callbacks.borrow_mut().abort();
                self.arena.borrow_mut().reset();
                return Err(error);
            }
        };

        self.callbacks.borrow_mut().commit();
        let arena = std::mem::take(&mut *self.arena.borrow_mut());
        Ok(RenderSnapshot::new(
            self,
            callbacks,
            root,
            arena,
            application,
            view.as_ref().map(Entity::downgrade),
        ))
    }

    #[cfg(test)]
    pub(crate) fn render_to_spec(
        self: &Rc<Self>,
        object: &ViewObject,
        view: Option<Entity<ScriptView>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<String> {
        let policy = view
            .as_ref()
            .map(|view| view.read(cx).policy())
            .unwrap_or_else(crate::policy::default);
        Ok(self
            .build_snapshot(object, view, policy, window, cx)?
            .debug_tree())
    }

    /// Releases the handlers registered while one snapshot was built.
    pub(crate) fn retire_callbacks(&self, generation: u64) {
        self.callbacks.borrow_mut().retire(generation);
    }

    /// Retires every callback registered by one retained view.
    pub(crate) fn retire_view_callbacks(&self, entity_id: EntityId) {
        self.callbacks.borrow_mut().retain(|entry| {
            entry
                .view
                .as_ref()
                .is_none_or(|owner| owner.entity_id() != entity_id)
        });
    }

    #[cfg(test)]
    pub(crate) fn live_callbacks(&self) -> usize {
        self.callbacks.borrow().len()
    }

    /// Puts a guest handler into the open callback generation, owned by the
    /// current view and application.
    fn register(&self, guest: &Rc<Guest>, handler: u32) -> CallbackId {
        self.callbacks.borrow_mut().push(CallbackEntry {
            value: GuestHandler {
                guest: Rc::downgrade(guest),
                handler,
            },
            view: scope::current_view().map(|view| view.downgrade()),
            application: scope::current_application_generation(),
            registered_in: scope::current_generation(),
        })
    }

    /// A live, current callback: its entry and the guest it calls.
    fn resolve(&self, id: CallbackId) -> Result<(CallbackEntry<GuestHandler>, Rc<Guest>)> {
        let entry = self
            .callbacks
            .borrow()
            .get(id)
            .ok_or_else(|| anyhow!("callback {id} belongs to a superseded render pass"))?;
        anyhow::ensure!(
            entry
                .application
                .as_ref()
                .is_none_or(|application| application.is_active()),
            "callback {id} belongs to a retired application"
        );
        let guest = entry
            .value
            .guest
            .upgrade()
            .ok_or_else(|| anyhow!("callback {id} belongs to a released guest"))?;
        Ok((entry, guest))
    }

    /// Runs one guest handler with `payload` under an event scope of its own.
    fn run_handler(
        self: &Rc<Self>,
        id: CallbackId,
        payload: Json,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<()> {
        let (entry, guest) = self.resolve(id)?;
        let view = entry
            .live_view()
            .ok_or_else(|| anyhow!("callback {id} owner has been released"))?;
        let policy = view
            .as_ref()
            .map(|view| view.read(cx).policy())
            .unwrap_or_else(crate::policy::default);
        let (_guard, _) = scope::enter_with_application(
            self,
            window,
            cx,
            ScopePhase::Event,
            view,
            policy,
            entry.application.clone(),
        );
        guest::handle(self, &guest, entry.value.handler, &payload)
    }

    /// [`Self::run_handler`] for an event whose failure has nobody to report
    /// to: a stale callback is expected, a guest trap is logged.
    fn dispatch(
        self: &Rc<Self>,
        id: CallbackId,
        what: &str,
        payload: Json,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Err(error) = self.run_handler(id, payload, window, cx) {
            tracing::debug!("{what} handler {id}: {error:#}");
        }
    }

    /// Describes one window of a lazy list's items, from inside layout. The
    /// guarantees are the QuickJS engine's: a `Layout` scope, an arena of its
    /// own, and the rows' handlers in a callback generation of their own.
    pub(crate) fn render_virtual_items(
        self: &Rc<Self>,
        id: CallbackId,
        get_key: CallbackId,
        range: std::ops::Range<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<crate::spec::ItemSpecs> {
        let (entry, guest) = self.resolve(id).ok()?;
        let (key_entry, _) = self.resolve(get_key).ok()?;
        let view = entry.live_view()?;
        let policy = view
            .as_ref()
            .map(|view| view.read(cx).policy())
            .unwrap_or_else(crate::policy::default);
        let (_guard, _) = scope::enter_with_application(
            self,
            window,
            cx,
            ScopePhase::Layout,
            view,
            policy,
            entry.application.clone(),
        );
        scope::adopt(entry.registered_in);

        let recording = !self.callbacks.borrow().is_building();
        let callback_generation = recording.then(|| self.callbacks.borrow_mut().begin());
        struct Recording<'a> {
            runtime: &'a ShellRuntime,
            open: bool,
        }
        impl Drop for Recording<'_> {
            fn drop(&mut self) {
                if self.open {
                    self.runtime.callbacks.borrow_mut().abort();
                }
            }
        }
        let mut recording = Recording {
            runtime: self,
            open: recording,
        };

        let outer = std::mem::take(&mut *self.arena.borrow_mut());
        let described = (|| -> Result<_> {
            let (start, end) = (range.start as u32, range.end as u32);
            let items = guest.render_items(entry.value.handler, start, end)?;
            let ids = guest::record_nodes(self, &guest, &items.nodes)?;
            let roots = items
                .roots
                .iter()
                .map(|root| {
                    ids.get(*root as usize)
                        .copied()
                        .ok_or_else(|| anyhow!("an item root is out of range"))
                })
                .collect::<Result<smallvec::SmallVec<_>>>()?;
            let mut keys = Vec::with_capacity(range.len());
            let mut unique = std::collections::HashSet::new();
            for index in range.clone() {
                let key = guest.item_key(key_entry.value.handler, index as u32)?;
                anyhow::ensure!(
                    unique.insert(key.clone()),
                    "list get_key returned duplicate key `{key}` in one visible range"
                );
                keys.push(key);
            }
            Ok((roots, keys))
        })();
        let arena = std::mem::replace(&mut *self.arena.borrow_mut(), outer);

        match described {
            Ok((roots, keys)) => match callback_generation {
                Some(generation) if self.callbacks.borrow().has_staged() => {
                    self.callbacks.borrow_mut().commit();
                    recording.open = false;
                    let snapshot = RenderSnapshot::new(
                        self,
                        generation,
                        roots.first().copied().unwrap_or_default(),
                        arena,
                        entry.application.clone(),
                        entry.view.clone(),
                    );
                    Some(crate::spec::ItemSpecs::recorded(snapshot, roots, keys))
                }
                _ => Some(crate::spec::ItemSpecs::new(arena, roots, keys)),
            },
            Err(error) => {
                tracing::error!("error in a guest list's item renderer: {error:#}");
                None
            }
        }
    }

    /// Asks a lazy list's key function for item keys by index, from inside
    /// layout. Once it fails, every later lookup answers `None`.
    pub(crate) fn item_keys<R>(
        self: &Rc<Self>,
        get_key: CallbackId,
        window: &mut Window,
        cx: &mut App,
        ask: impl FnOnce(&mut dyn FnMut(usize) -> Option<String>) -> R,
    ) -> Option<R> {
        let (entry, guest) = self.resolve(get_key).ok()?;
        let view = entry.live_view()?;
        let policy = view
            .as_ref()
            .map(|view| view.read(cx).policy())
            .unwrap_or_else(crate::policy::default);
        self.metrics.time_frame_script(|| {
            let (_guard, _) = scope::enter_with_application(
                self,
                window,
                cx,
                ScopePhase::Layout,
                view,
                policy,
                entry.application.clone(),
            );
            scope::adopt(entry.registered_in);
            let mut failed = false;
            let answer = ask(&mut |index| {
                if failed {
                    return None;
                }
                match guest.item_key(entry.value.handler, index as u32) {
                    Ok(key) => Some(key),
                    Err(error) => {
                        tracing::error!("error in a guest list's get_key: {error:#}");
                        failed = true;
                        None
                    }
                }
            });
            Some(answer)
        })
    }

    pub(crate) fn dispatch_item_key(
        self: &Rc<Self>,
        id: CallbackId,
        key: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.dispatch(id, "item key", json!({ "key": key }), window, cx);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dispatch_item_mouse_button(
        self: &Rc<Self>,
        id: CallbackId,
        key: &str,
        button: gpui::MouseButton,
        position: gpui::Point<gpui::Pixels>,
        click_count: usize,
        modifiers: gpui::Modifiers,
        bounds: Option<gpui::Bounds<gpui::Pixels>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mut payload = mouse_button_payload(button, position, click_count, modifiers, bounds);
        payload["key"] = json!(key);
        self.dispatch(id, "item mouse", payload, window, cx);
    }

    pub(crate) fn dispatch_visible_range(
        self: &Rc<Self>,
        id: CallbackId,
        range: std::ops::Range<usize>,
        count: usize,
        window: &mut Window,
        cx: &mut App,
    ) {
        let payload = json!({ "start": range.start, "end": range.end, "count": count });
        self.dispatch(id, "visible range", payload, window, cx);
    }

    pub(crate) fn dispatch_click(
        self: &Rc<Self>,
        id: CallbackId,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut App,
    ) {
        let payload = json!({
            "click_count": event.click_count(),
            "modifiers": modifiers_payload(event.modifiers()),
        });
        self.dispatch(id, "click", payload, window, cx);
    }

    pub(crate) fn dispatch_resize(
        self: &Rc<Self>,
        id: CallbackId,
        sizes: Vec<f32>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.dispatch(id, "resize", json!(sizes), window, cx);
    }

    pub(crate) fn dispatch_mouse_move(
        self: &Rc<Self>,
        id: CallbackId,
        event: &gpui::MouseMoveEvent,
        local: gpui::Point<gpui::Pixels>,
        bounds: gpui::Bounds<gpui::Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let payload = json!({
            "position": point_payload(event.position),
            "local_position": point_payload(local),
            "bounds": bounds_payload(bounds),
            "pressed_button": event.pressed_button.map(button_name),
            "modifiers": modifiers_payload(event.modifiers),
        });
        self.dispatch(id, "mouse move", payload, window, cx);
    }

    pub(crate) fn dispatch_action(
        self: &Rc<Self>,
        id: CallbackId,
        action: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.dispatch(id, "action", json!(action), window, cx);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dispatch_mouse_button(
        self: &Rc<Self>,
        id: CallbackId,
        button: gpui::MouseButton,
        position: gpui::Point<gpui::Pixels>,
        click_count: usize,
        modifiers: gpui::Modifiers,
        bounds: Option<gpui::Bounds<gpui::Pixels>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let payload = mouse_button_payload(button, position, click_count, modifiers, bounds);
        self.dispatch(id, "mouse button", payload, window, cx);
    }

    pub(crate) fn dispatch_scroll_wheel(
        self: &Rc<Self>,
        id: CallbackId,
        event: &gpui::ScrollEvent,
        line_height: gpui::Pixels,
        bounds: Option<gpui::Bounds<gpui::Pixels>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let delta = event.delta.pixel_delta(line_height);
        let payload = json!({
            "position": point_payload(event.position),
            "delta": point_payload(delta),
            "bounds": bounds.map(bounds_payload),
            "modifiers": modifiers_payload(event.modifiers),
        });
        self.dispatch(id, "scroll wheel", payload, window, cx);
    }

    pub(crate) fn dispatch_pinch(
        self: &Rc<Self>,
        id: CallbackId,
        event: &gpui::PinchEvent,
        bounds: Option<gpui::Bounds<gpui::Pixels>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let payload = json!({
            "position": point_payload(event.position),
            "delta": event.delta,
            "bounds": bounds.map(bounds_payload),
            "modifiers": modifiers_payload(event.modifiers),
        });
        self.dispatch(id, "pinch", payload, window, cx);
    }

    pub(crate) fn dispatch_modifiers_changed(
        self: &Rc<Self>,
        id: CallbackId,
        event: &gpui::ModifiersChangedEvent,
        window: &mut Window,
        cx: &mut App,
    ) {
        let payload = json!({ "modifiers": modifiers_payload(event.modifiers) });
        self.dispatch(id, "modifiers", payload, window, cx);
    }

    pub(crate) fn dispatch_key(
        self: &Rc<Self>,
        id: CallbackId,
        keystroke: &gpui::Keystroke,
        is_held: Option<bool>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let payload = json!({
            "key": keystroke.key,
            "key_char": keystroke.key_char,
            "modifiers": modifiers_payload(keystroke.modifiers),
            "is_held": is_held,
        });
        self.dispatch(id, "key", payload, window, cx);
    }

    pub(crate) fn dispatch_change(
        self: &Rc<Self>,
        id: CallbackId,
        checked: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.dispatch(id, "change", json!(checked), window, cx);
    }

    pub(crate) fn dispatch_host_event(
        self: &Rc<Self>,
        id: CallbackId,
        payload: HostValue,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.dispatch(id, "host component", host_value_json(payload), window, cx);
    }

    pub(crate) fn dispatch_step(
        self: &Rc<Self>,
        id: CallbackId,
        action: &'static str,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.dispatch(id, "step", json!(action), window, cx);
    }

    pub(crate) fn dispatch_signal(
        self: &Rc<Self>,
        id: CallbackId,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.dispatch(id, "signal", json!({}), window, cx);
    }

    /// A component's event handler. A guest handler reports only whether it
    /// changed state, so the component reads no value back.
    pub(crate) fn dispatch_component_callback_value(
        self: &Rc<Self>,
        id: CallbackId,
        arguments: &[ComponentCallbackArgument],
        window: &mut Window,
        cx: &mut App,
    ) -> Result<ComponentCallbackValue> {
        let payload = arguments.first().map_or(Json::Null, callback_argument_json);
        self.run_handler(id, payload, window, cx)?;
        Ok(ComponentCallbackValue::Null)
    }

    pub(crate) fn dispatch_component_event_data(
        self: &Rc<Self>,
        id: CallbackId,
        arguments: &[ComponentDataValue],
        window: &mut Window,
        cx: &mut App,
    ) -> Result<ComponentCallbackValue> {
        let payload = arguments.first().map_or(Json::Null, data_json);
        self.run_handler(id, payload, window, cx)?;
        Ok(ComponentCallbackValue::Null)
    }

    /// A component asking for data during layout (a table's cell, a tree's
    /// row). A guest has no handler that returns a value yet.
    pub(crate) fn dispatch_component_data_callback(
        self: &Rc<Self>,
        id: CallbackId,
        _arguments: &[ComponentCallbackArgument],
        _window: &mut Window,
        _cx: &mut App,
    ) -> Result<ComponentDataValue> {
        Err(anyhow!(
            "component callback {id}: a WebAssembly guest cannot return component data"
        ))
    }

    /// A component asking for an element during layout. As above.
    pub(crate) fn dispatch_component_element_callback(
        self: &Rc<Self>,
        id: CallbackId,
        _arguments: &[ComponentCallbackArgument],
        _window: &mut Window,
        _cx: &mut App,
    ) -> Result<Option<gpui::AnyElement>> {
        Err(anyhow!(
            "component callback {id}: a WebAssembly guest cannot render component elements"
        ))
    }

    pub(crate) fn dispatch_component_element_data_callback(
        self: &Rc<Self>,
        id: CallbackId,
        arguments: &[ComponentDataValue],
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Option<gpui::AnyElement>> {
        self.dispatch_inline_element_data(id, arguments, false, window, cx)
    }

    pub(crate) fn dispatch_inline_element_data(
        self: &Rc<Self>,
        id: CallbackId,
        _arguments: &[ComponentDataValue],
        _interactive: bool,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Result<Option<gpui::AnyElement>> {
        Err(anyhow!(
            "component callback {id}: a WebAssembly guest cannot render component elements"
        ))
    }

    /// Runs a component's native window effect under the event scope its
    /// handler was registered with.
    pub(crate) fn with_component_callback_event<R>(
        self: &Rc<Self>,
        id: CallbackId,
        window: &mut Window,
        cx: &mut App,
        body: impl FnOnce(&mut Window, &mut App) -> Result<R>,
    ) -> Result<R> {
        if let Some(phase) = scope::current_phase() {
            anyhow::ensure!(
                phase.allows_notify(),
                "component window effects are not allowed during the `{}` phase",
                phase.as_str()
            );
        }
        let (entry, _) = self.resolve(id)?;
        let view = entry
            .live_view()
            .ok_or_else(|| anyhow!("component callback {id} owner has been released"))?;
        let policy = view
            .as_ref()
            .map(|view| view.read(cx).policy())
            .unwrap_or_else(crate::policy::default);
        let (_guard, _) = scope::enter_with_application(
            self,
            window,
            cx,
            ScopePhase::Event,
            view,
            policy,
            entry.application,
        );
        body(window, cx)
    }
}

fn modifiers_payload(modifiers: gpui::Modifiers) -> Json {
    json!({
        "shift": modifiers.shift,
        "control": modifiers.control,
        "alt": modifiers.alt,
        "platform": modifiers.platform,
        "function": modifiers.function,
    })
}

fn point_payload(point: gpui::Point<gpui::Pixels>) -> Json {
    json!({ "x": f32::from(point.x), "y": f32::from(point.y) })
}

fn bounds_payload(bounds: gpui::Bounds<gpui::Pixels>) -> Json {
    json!({
        "x": f32::from(bounds.origin.x),
        "y": f32::from(bounds.origin.y),
        "width": f32::from(bounds.size.width),
        "height": f32::from(bounds.size.height),
    })
}

fn button_name(button: gpui::MouseButton) -> &'static str {
    match button {
        gpui::MouseButton::Left => "left",
        gpui::MouseButton::Right => "right",
        gpui::MouseButton::Middle => "middle",
        gpui::MouseButton::Navigate(gpui::NavigationDirection::Back) => "back",
        gpui::MouseButton::Navigate(gpui::NavigationDirection::Forward) => "forward",
    }
}

fn mouse_button_payload(
    button: gpui::MouseButton,
    position: gpui::Point<gpui::Pixels>,
    click_count: usize,
    modifiers: gpui::Modifiers,
    bounds: Option<gpui::Bounds<gpui::Pixels>>,
) -> Json {
    json!({
        "button": button_name(button),
        "position": point_payload(position),
        "click_count": click_count,
        "modifiers": modifiers_payload(modifiers),
        "bounds": bounds.map(bounds_payload),
    })
}

fn host_value_json(value: HostValue) -> Json {
    match value {
        HostValue::Null => Json::Null,
        HostValue::Bool(value) => json!(value),
        HostValue::Number(value) => json!(value),
        HostValue::Str(value) => json!(value),
        HostValue::Array(values) => Json::Array(values.into_iter().map(host_value_json).collect()),
        HostValue::Object(entries) => Json::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key, host_value_json(value)))
                .collect(),
        ),
    }
}

fn callback_argument_json(argument: &ComponentCallbackArgument) -> Json {
    match argument {
        ComponentCallbackArgument::String(value) => json!(value),
        ComponentCallbackArgument::Number(value) => json!(value),
        ComponentCallbackArgument::Boolean(value) => json!(value),
        ComponentCallbackArgument::Array(values) => {
            Json::Array(values.iter().map(callback_argument_json).collect())
        }
    }
}

fn data_json(value: &ComponentDataValue) -> Json {
    match value {
        ComponentDataValue::Null => Json::Null,
        ComponentDataValue::Boolean(value) => json!(value),
        ComponentDataValue::Number(value) => json!(value),
        ComponentDataValue::String(value) => json!(value),
        ComponentDataValue::Array(values) => Json::Array(values.iter().map(data_json).collect()),
        ComponentDataValue::Object(entries) => Json::Object(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), data_json(value)))
                .collect(),
        ),
    }
}

/// The work guests started, each owned by the view, application generation
/// and policy it began under.
struct OwnedTask {
    _task: gpui::Task<()>,
    view: Option<EntityId>,
    application: Option<Weak<ApplicationGeneration>>,
    policy: Weak<Policy>,
}

thread_local! {
    static TASKS: RefCell<HashMap<u64, OwnedTask>> = RefCell::new(HashMap::new());
    static NEXT_TASK: Cell<u64> = const { Cell::new(0) };
}

/// Keeps the task `spawn` starts until it finishes or one of its owners goes
/// away. The task calls [`finish_task`] with the id it is handed as it ends.
pub(crate) fn own_task(spawn: impl FnOnce(u64) -> gpui::Task<()>) {
    let id = NEXT_TASK.with(|next| {
        let id = next.get();
        next.set(id + 1);
        id
    });
    let task = spawn(id);
    let owned = OwnedTask {
        _task: task,
        view: scope::current_view().map(|view| view.entity_id()),
        application: scope::current_application_generation().map(|owner| Rc::downgrade(&owner)),
        policy: Rc::downgrade(&scope::policy()),
    };
    TASKS.with(|tasks| tasks.borrow_mut().insert(id, owned));
}

/// Forgets a task that finished on its own.
pub(crate) fn finish_task(id: u64) {
    // Taken out and dropped after the borrow: dropping a task can run code.
    let finished = TASKS.with(|tasks| tasks.borrow_mut().remove(&id));
    drop(finished);
}

fn cancel_tasks(matches: impl Fn(&OwnedTask) -> bool) {
    let cancelled: Vec<_> = TASKS.with(|tasks| {
        let mut tasks = tasks.borrow_mut();
        let ids: Vec<u64> = tasks
            .iter()
            .filter(|(_, task)| matches(task))
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter().filter_map(|id| tasks.remove(&id)).collect()
    });
    drop(cancelled);
}

pub(crate) fn cancel_policy_tasks(policy: &Rc<Policy>) {
    cancel_tasks(|task| std::ptr::eq(task.policy.as_ptr(), Rc::as_ptr(policy)));
}

pub(crate) fn cancel_application_tasks(generation: &Rc<ApplicationGeneration>) {
    cancel_tasks(|task| {
        task.application
            .as_ref()
            .is_some_and(|owner| std::ptr::eq(owner.as_ptr(), Rc::as_ptr(generation)))
    });
}

pub(crate) fn cancel_view_tasks(_runtime: &Rc<ShellRuntime>, entity_id: EntityId) {
    cancel_tasks(|task| task.view == Some(entity_id));
}

#[cfg(test)]
pub(crate) fn task_count() -> usize {
    TASKS.with(|tasks| tasks.borrow().len())
}

/// A guest is sandboxed by WebAssembly and by its grants, and a development
/// session relaxes neither, so there is nothing here for the switch to turn.
pub(crate) fn set_development_mode(_enabled: bool) {}

/// The module names a script imports from. A guest imports none, so the
/// generated agent notes list nothing under them.
pub(crate) mod exports {
    pub(crate) const GPUI: &[&str] = &[];
    pub(crate) const GPUI_BASE: &[&str] = &[];
    pub(crate) const GPUI_FPS: &[&str] = &[];
}
