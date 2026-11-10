//! The engine-independent half of hosting registered components: their
//! retained state and the application effects they install.
//!
//! Neither touches a script value, so both engines keep these in their own
//! `ShellRuntime` fields and call through here rather than each carrying a
//! copy. The effect callbacks reach the runtime back through
//! [`ShellRuntime::component_app_effects`], which every engine provides.

use std::{
    cell::RefCell,
    collections::HashMap,
    rc::{Rc, Weak},
};

use anyhow::anyhow;
use gpui::{App, Subscription, WeakEntity, Window};

use crate::{
    component_registry::{ComponentAppEffectCleanup, RetainedStateStore},
    engine::ShellRuntime,
    runtime::ApplicationGeneration,
    view::ScriptView,
};

pub(crate) type AppEffectInstall = Box<dyn FnOnce(&mut App) -> ComponentAppEffectCleanup>;

pub(crate) struct InstalledAppEffect {
    revision: String,
    cleanup: Option<ComponentAppEffectCleanup>,
}

pub(crate) struct ComponentAppEffectGeneration {
    application: Rc<ApplicationGeneration>,
    /// The root view the effects were installed for. Retiring a generation
    /// runs its cleanups through this view, so a reload does not have to wait
    /// for the window to close before native state is restored.
    view: WeakEntity<ScriptView>,
    pending: HashMap<String, String>,
    installed: HashMap<String, InstalledAppEffect>,
    _release: Subscription,
}

pub(crate) type AppEffects = RefCell<HashMap<usize, ComponentAppEffectGeneration>>;

fn queue_component_app_effect(
    pending: &mut HashMap<String, String>,
    installed: &HashMap<String, InstalledAppEffect>,
    key: &str,
    revision: &str,
) -> bool {
    if pending.get(key).is_some_and(|value| value == revision) {
        return false;
    }
    if installed
        .get(key)
        .is_some_and(|effect| effect.revision == revision)
    {
        pending.remove(key);
        return false;
    }
    pending.insert(key.to_owned(), revision.to_owned());
    true
}

/// Releases queued for a store that was borrowed when they were asked for.
pub(crate) fn flush_state_releases(
    states: &RefCell<RetainedStateStore>,
    pending: &RefCell<Vec<Rc<ApplicationGeneration>>>,
) {
    loop {
        let queued = std::mem::take(&mut *pending.borrow_mut());
        if queued.is_empty() {
            return;
        }
        let Ok(mut states) = states.try_borrow_mut() else {
            pending.borrow_mut().extend(queued);
            return;
        };
        for application in queued {
            states.release_application(&application);
        }
    }
}

pub(crate) fn with_state<T: std::any::Any, R>(
    states: &RefCell<RetainedStateStore>,
    pending: &RefCell<Vec<Rc<ApplicationGeneration>>>,
    handle: u64,
    kind: &'static str,
    body: impl FnOnce(&T) -> R,
) -> anyhow::Result<R> {
    flush_state_releases(states, pending);
    let result = states
        .try_borrow()
        .map_err(|_| anyhow!("retained component state is already mutably borrowed"))?
        .with(handle, kind, body);
    flush_state_releases(states, pending);
    result
}

pub(crate) fn update_state<T: std::any::Any, R>(
    states: &RefCell<RetainedStateStore>,
    pending: &RefCell<Vec<Rc<ApplicationGeneration>>>,
    handle: u64,
    kind: &'static str,
    window: &mut Window,
    cx: &mut App,
    body: impl FnOnce(&mut T, &mut Window, &mut App) -> R,
) -> anyhow::Result<R> {
    flush_state_releases(states, pending);
    let result = states
        .try_borrow_mut()
        .map_err(|_| anyhow!("retained component state is already borrowed"))?
        .with_mut(handle, kind, |state| body(state, window, cx));
    flush_state_releases(states, pending);
    result
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn schedule_app_effect(
    runtime: &Rc<ShellRuntime>,
    application: Rc<ApplicationGeneration>,
    view: WeakEntity<ScriptView>,
    key: String,
    revision: String,
    window: &mut Window,
    cx: &mut App,
    install: AppEffectInstall,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        application.is_active(),
        "component app effect belongs to a retired application"
    );
    let view = view
        .upgrade()
        .ok_or_else(|| anyhow!("component app effects require a live root view"))?;
    let application_key = Rc::as_ptr(&application) as usize;

    let mut generations = runtime.component_app_effects().borrow_mut();
    if let std::collections::hash_map::Entry::Vacant(slot) = generations.entry(application_key) {
        let weak = Rc::downgrade(runtime);
        let release = cx.observe_release(&view, move |_, cx| {
            if let Some(runtime) = weak.upgrade() {
                cleanup_app_effects(runtime.component_app_effects(), application_key, cx);
            }
        });
        slot.insert(ComponentAppEffectGeneration {
            application: application.clone(),
            view: view.downgrade(),
            pending: HashMap::new(),
            installed: HashMap::new(),
            _release: release,
        });
    }
    let generation = generations
        .get_mut(&application_key)
        .expect("inserted above");
    if !queue_component_app_effect(
        &mut generation.pending,
        &generation.installed,
        &key,
        &revision,
    ) {
        return Ok(());
    }
    drop(generations);

    let weak: Weak<ShellRuntime> = Rc::downgrade(runtime);
    window.defer(cx, move |_, cx| {
        let Some(runtime) = weak.upgrade() else {
            return;
        };
        apply_app_effect(
            runtime.component_app_effects(),
            application_key,
            key,
            revision,
            install,
            cx,
        );
    });
    Ok(())
}

fn apply_app_effect(
    effects: &AppEffects,
    application_key: usize,
    key: String,
    revision: String,
    install: AppEffectInstall,
    cx: &mut App,
) {
    let old_cleanup = {
        let mut generations = effects.borrow_mut();
        let Some(generation) = generations.get_mut(&application_key) else {
            return;
        };
        if !generation.application.is_active() || generation.pending.get(&key) != Some(&revision) {
            return;
        }
        generation.pending.remove(&key);
        generation
            .installed
            .remove(&key)
            .and_then(|mut effect| effect.cleanup.take())
    };
    if let Some(cleanup) = old_cleanup {
        cleanup(cx);
    }
    let cleanup = install(cx);
    if let Some(generation) = effects.borrow_mut().get_mut(&application_key) {
        generation.installed.insert(
            key,
            InstalledAppEffect {
                revision,
                cleanup: Some(cleanup),
            },
        );
    } else {
        cleanup(cx);
    }
}

/// Runs the cleanups an application generation installed, at the moment that
/// generation retires.
///
/// Without this, a reload would leave the previous generation's native state
/// (menus, globals, window chrome) installed until the root view was
/// released, and the replacing generation could not reach it: keys are scoped
/// per generation, so the new install adds to the old rather than replacing
/// it. The release subscription remains the backstop for paths that have no
/// `App` to run cleanups against.
pub(crate) fn retire_app_effects(
    effects: &AppEffects,
    application: &Rc<ApplicationGeneration>,
    cx: &mut impl gpui::AppContext,
) {
    let application_key = Rc::as_ptr(application) as usize;
    let Some(generation) = effects.borrow_mut().remove(&application_key) else {
        return;
    };
    let Some(view) = generation.view.upgrade() else {
        // The view is already gone, so the release subscription has run.
        return;
    };
    let mut generation = generation;
    view.update(cx, |_, cx| {
        for (_, mut effect) in generation.installed.drain() {
            if let Some(cleanup) = effect.cleanup.take() {
                cleanup(cx);
            }
        }
    });
}

fn cleanup_app_effects(effects: &AppEffects, application_key: usize, cx: &mut App) {
    let Some(mut generation) = effects.borrow_mut().remove(&application_key) else {
        return;
    };
    for (_, mut effect) in generation.installed.drain() {
        if let Some(cleanup) = effect.cleanup.take() {
            cleanup(cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returning_to_the_installed_app_effect_cancels_a_pending_replacement() {
        let key = "menu".to_owned();
        let mut pending = HashMap::new();
        let mut installed = HashMap::from([(
            key.clone(),
            InstalledAppEffect {
                revision: "a".into(),
                cleanup: None,
            },
        )]);

        assert!(queue_component_app_effect(
            &mut pending,
            &installed,
            &key,
            "b"
        ));
        assert_eq!(pending.get(&key).map(String::as_str), Some("b"));
        assert!(!queue_component_app_effect(
            &mut pending,
            &installed,
            &key,
            "a"
        ));
        assert!(!pending.contains_key(&key));

        installed.get_mut(&key).unwrap().revision = "b".into();
        assert!(queue_component_app_effect(
            &mut pending,
            &installed,
            &key,
            "a"
        ));
    }
}
