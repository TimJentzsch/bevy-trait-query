//! An implementation of trait queries for the bevy game engine.
//!
//! Before using this crate, you should be familiar with bevy: https://bevyengine.org/.
//! The current published version depends on bevy 0.8, although there is a branch on github
//! that supports the upcoming version.
//!
//! # Note on reliability
//!
//! This crate is experimental, and not battle-tested. It seems to work in my personal testing,
//! but it very well could contain undefined behavior. Use with caution (and miri!).
//!
//! If you find a bug, please [open an issue](https://github.com/JoJoJet/bevy-trait-query/issues).
//!
//! # Overview
//!
//! `bevy-trait-query` extends the capabilities of `bevy` by allowing you to query for components implementing a trait.
//!
//! ```
//! use bevy::prelude::*;
//!
//! // Some trait that we wish to use in queries.
//! pub trait Tooltip: 'static {
//!     fn tooltip(&self) -> &str;
//! }
//!
//! // Add the necessary impls for querying.
//! bevy_trait_query::impl_trait_query!(Tooltip);
//!
//! // Define some custom components which will implement the trait.
//!
//! #[derive(Component)]
//! struct Person(String);
//!
//! #[derive(Component)]
//! struct Monster;
//!
//! impl Tooltip for Person {
//!     fn tooltip(&self) -> &str {
//!         &self.0
//!     }
//! }
//!
//! impl Tooltip for Monster {
//!     fn tooltip(&self) -> &str {
//!         "Run!"
//!     }
//! }
//!
//! fn main() {
//!     // We must import this trait in order to register our trait impls.
//!     // If we don't register them, they will be invisible to the game engine.
//!     use bevy_trait_query::RegisterExt;
//!
//!     App::new()
//!         // Register our components.
//!         .register_component_as::<dyn Tooltip, Person>()
//!         .register_component_as::<dyn Tooltip, Monster>()
//!         .add_startup_system(setup)
//!         .add_system(show_tooltips)
//!         .add_system(show_tooltips_one)
//!         # .update();
//! }
//!
//! fn setup(mut commands: Commands) {
//!     commands.spawn().insert(Person("Fourier".to_owned()));
//!     commands.spawn().insert(Monster);
//! }
//!
//! fn show_tooltips(
//!     // Query for entities with components implementing the trait.
//!     query: Query<&dyn Tooltip>,
//! ) {
//!     for entity_tooltips in &query {
//!         // It's possible for an entity to have more than one component implementing the trait,
//!         // so we must iterate over all possible components for each entity.
//!         for tooltip in entity_tooltips {
//!             println!("Hovering: {}", tooltip.tooltip());
//!         }
//!     }
//! }
//!
//! use bevy_trait_query::One;
//! fn show_tooltips_one(
//!     // If you expect to only have one trait impl per entity, you should use the `One` filter.
//!     // This is significantly more efficient than iterating over all trait impls.
//!     query: Query<One<&dyn Tooltip>>,
//! ) {
//!     for tooltip in &query {
//!         println!("Hovering: {}", tooltip.tooltip());
//!     }
//! }
//! ```
//!
//! # Performance
//!
//! The performance of trait queries is quite competitive. Here are some benchmarks for simple cases:
//!
//! |                   | Concrete type | One<dyn Trait> | All<dyn Trait> |
//! |-------------------|----------------|-------------------|-----------------|
//! | 1 match           | 16.135 µs      | 31.441 µs         | 63.273 µs       |
//! | 2 matches         | 17.501 µs      | -                 | 102.83 µs       |
//! | 1-2 matches       | -              | 16.959 µs         | 82.179 µs       |
//!
//! # Poor use cases
//!
//! You should avoid using trait queries for very simple cases that can be solved with more direct solutions.
//!
//! One naive use would be querying for a trait that looks something like:
//!
//! ```
//! trait Person {
//!     fn name(&self) -> &str;
//! }
//! ```
//!
//! A far better way of expressing this would be to store the name in a separate component
//! and query for that directly, making `Person` a simple marker component.
//!
//! Trait queries are often the most *obvious* solution to a problem, but not always the best one.
//! For examples of strong real-world use-cases, check out the RFC for trait queries in `bevy`:
//! https://github.com/bevyengine/rfcs/pull/39.
//!

use std::{cell::UnsafeCell, marker::PhantomData};

use bevy::{
    ecs::{
        component::{ComponentId, ComponentTicks, StorageType},
        query::{Fetch, FetchState, ReadOnlyWorldQuery, WorldQuery, WorldQueryGats},
        storage::{ComponentSparseSet, SparseSets, Table},
    },
    prelude::*,
    ptr::{Ptr, PtrMut, ThinSlicePtr, UnsafeCellDeref},
};
use change_detection::{Mut, Ticks};

#[cfg(test)]
mod tests;

pub mod change_detection;

/// Marker for traits that can be used in queries.
pub trait TraitQuery: 'static {}

#[doc(hidden)]
pub trait TraitQueryMarker<Trait: ?Sized + TraitQuery> {
    type Covered: Component;
    /// Casts an untyped pointer to a trait object pointer,
    /// with a vtable corresponding to `Self::Covered`.
    fn cast(_: *mut u8) -> *mut Trait;
}

/// Extension methods for registering components with trait queries.
pub trait RegisterExt {
    fn register_component_as<Trait: ?Sized + TraitQuery, C: Component>(&mut self) -> &mut Self
    where
        (C,): TraitQueryMarker<Trait, Covered = C>;
}

impl RegisterExt for World {
    fn register_component_as<Trait: ?Sized + TraitQuery, C: Component>(&mut self) -> &mut Self
    where
        (C,): TraitQueryMarker<Trait, Covered = C>,
    {
        let component_id = self.init_component::<C>();
        let registry = self
            .get_resource_or_insert_with::<TraitImplRegistry<Trait>>(default)
            .into_inner();
        let meta = TraitImplMeta {
            size_bytes: std::mem::size_of::<C>(),
            dyn_ctor: DynCtor { cast: <(C,)>::cast },
        };
        registry.register::<C>(component_id, meta);
        self
    }
}

impl RegisterExt for App {
    fn register_component_as<Trait: ?Sized + TraitQuery, C: Component>(&mut self) -> &mut Self
    where
        (C,): TraitQueryMarker<Trait, Covered = C>,
    {
        self.world.register_component_as::<Trait, C>();
        self
    }
}

struct TraitImplRegistry<Trait: ?Sized> {
    // Component IDs are stored contiguously so that we can search them quickly.
    components: Vec<ComponentId>,
    meta: Vec<TraitImplMeta<Trait>>,

    table_components: Vec<ComponentId>,
    table_meta: Vec<TraitImplMeta<Trait>>,

    sparse_components: Vec<ComponentId>,
    sparse_meta: Vec<TraitImplMeta<Trait>>,

    sealed: bool,
}

impl<T: ?Sized> Default for TraitImplRegistry<T> {
    #[inline]
    fn default() -> Self {
        Self {
            components: vec![],
            meta: vec![],
            table_components: vec![],
            table_meta: vec![],
            sparse_components: vec![],
            sparse_meta: vec![],
            sealed: false,
        }
    }
}

impl<Trait: ?Sized + TraitQuery> TraitImplRegistry<Trait> {
    fn register<C: Component>(&mut self, component: ComponentId, meta: TraitImplMeta<Trait>) {
        // Don't register the same component multiple times.
        if self.components.contains(&component) {
            return;
        }

        if self.sealed {
            // It is not possible to update the `FetchState` for a given system after the game has started,
            // so for explicitness, let's panic instead of having a trait impl silently get forgotten.
            panic!("Cannot register new trait impls after the game has started");
        }

        self.components.push(component);
        self.meta.push(meta);

        use bevy::ecs::component::ComponentStorage;
        match <C as Component>::Storage::STORAGE_TYPE {
            StorageType::Table => {
                self.table_components.push(component);
                self.table_meta.push(meta);
            }
            StorageType::SparseSet => {
                self.sparse_components.push(component);
                self.sparse_meta.push(meta);
            }
        }
    }
    fn seal(&mut self) {
        self.sealed = true;
    }
}

/// Stores data about an impl of a trait
struct TraitImplMeta<Trait: ?Sized> {
    size_bytes: usize,
    dyn_ctor: DynCtor<Trait>,
}

impl<T: ?Sized> Copy for TraitImplMeta<T> {}
impl<T: ?Sized> Clone for TraitImplMeta<T> {
    fn clone(&self) -> Self {
        *self
    }
}

#[doc(hidden)]
pub mod imports {
    pub use bevy::ecs::{
        component::Component,
        query::{QueryItem, ReadOnlyWorldQuery, WorldQuery, WorldQueryGats},
    };
}

#[macro_export]
macro_rules! impl_trait_query {
    ($trait:ident) => {
        impl $crate::TraitQuery for dyn $trait {}

        impl<T: $trait + $crate::imports::Component> $crate::TraitQueryMarker<dyn $trait> for (T,) {
            type Covered = T;
            fn cast(ptr: *mut u8) -> *mut dyn $trait {
                ptr as *mut T as *mut _
            }
        }

        impl<'w> $crate::imports::WorldQueryGats<'w> for &dyn $trait {
            type Fetch = $crate::ReadAllTraitsFetch<'w, dyn $trait>;
            type _State = $crate::AllQueryState<dyn $trait>;
        }

        unsafe impl $crate::imports::ReadOnlyWorldQuery for &dyn $trait {}

        unsafe impl<'w> $crate::imports::WorldQuery for &'w dyn $trait {
            type ReadOnly = Self;
            type State = $crate::AllQueryState<dyn $trait>;

            fn shrink<'wlong: 'wshort, 'wshort>(
                item: $crate::imports::QueryItem<'wlong, Self>,
            ) -> $crate::imports::QueryItem<'wshort, Self> {
                item
            }
        }

        impl<'w> $crate::imports::WorldQueryGats<'w> for &mut dyn $trait {
            type Fetch = $crate::WriteAllTraitsFetch<'w, dyn $trait>;
            type _State = $crate::AllQueryState<dyn $trait>;
        }

        unsafe impl<'w> $crate::imports::WorldQuery for &'w mut dyn $trait {
            type ReadOnly = &'w dyn $trait;
            type State = $crate::AllQueryState<dyn $trait>;

            fn shrink<'wlong: 'wshort, 'wshort>(
                item: $crate::imports::QueryItem<'wlong, Self>,
            ) -> $crate::imports::QueryItem<'wshort, Self> {
                item
            }
        }
    };
}

#[doc(hidden)]
pub struct OneQueryState<Trait: ?Sized> {
    components: Box<[ComponentId]>,
    meta: Box<[TraitImplMeta<Trait>]>,
}

impl<Trait: ?Sized + TraitQuery> FetchState for OneQueryState<Trait> {
    fn init(world: &mut World) -> Self {
        #[cold]
        fn error<T: ?Sized + 'static>() -> ! {
            panic!(
                "no components found matching `{}`, did you forget to register them?",
                std::any::type_name::<T>()
            )
        }

        let mut registry = world
            .get_resource_mut::<TraitImplRegistry<Trait>>()
            .unwrap_or_else(|| error::<Trait>());
        registry.seal();
        Self {
            components: registry.components.clone().into_boxed_slice(),
            meta: registry.meta.clone().into_boxed_slice(),
        }
    }
    fn matches_component_set(&self, set_contains_id: &impl Fn(ComponentId) -> bool) -> bool {
        let match_count = self
            .components
            .iter()
            .filter(|&&c| set_contains_id(c))
            .count();
        match_count == 1
    }
}

/// Turns an untyped pointer into a trait object pointer,
/// for a specific erased concrete type.
struct DynCtor<Trait: ?Sized> {
    cast: unsafe fn(*mut u8) -> *mut Trait,
}

impl<T: ?Sized> Copy for DynCtor<T> {}
impl<T: ?Sized> Clone for DynCtor<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Trait: ?Sized> DynCtor<Trait> {
    unsafe fn cast(self, ptr: Ptr) -> &Trait {
        &*(self.cast)(ptr.as_ptr())
    }
    unsafe fn cast_mut(self, ptr: PtrMut) -> &mut Trait {
        &mut *(self.cast)(ptr.as_ptr())
    }
}

struct ZipExact<A, B> {
    a: A,
    b: B,
}

impl<A: Iterator, B: Iterator> Iterator for ZipExact<A, B> {
    type Item = (A::Item, B::Item);
    fn next(&mut self) -> Option<Self::Item> {
        let a = self.a.next()?;
        let b = self
            .b
            .next()
            // SAFETY: `a` returned a valid value, and the caller of `zip_exact`
            // guaranteed that `b` will return a value as long as `a` does.
            .unwrap_or_else(|| unsafe { debug_unreachable() });
        Some((a, b))
    }
}

/// SAFETY: `b` must yield at least as many items as `a`.
unsafe fn zip_exact<A: IntoIterator, B: IntoIterator>(
    a: A,
    b: B,
) -> ZipExact<A::IntoIter, B::IntoIter>
where
    A::IntoIter: ExactSizeIterator,
    B::IntoIter: ExactSizeIterator,
{
    let a = a.into_iter();
    let b = b.into_iter();
    debug_assert_eq!(a.len(), b.len());
    ZipExact { a, b }
}

/// [`WorldQuery`] adapter that fetches entities with exactly one component implementing a trait.
pub struct One<T>(pub T);

impl<'w, 'a, Trait: ?Sized + TraitQuery> WorldQueryGats<'w> for One<&'a Trait> {
    type Fetch = ReadTraitFetch<'w, Trait>;
    type _State = OneQueryState<Trait>;
}

unsafe impl<'a, Trait: ?Sized + TraitQuery> WorldQuery for One<&'a Trait> {
    type ReadOnly = Self;
    type State = OneQueryState<Trait>;

    fn shrink<'wlong: 'wshort, 'wshort>(
        item: bevy::ecs::query::QueryItem<'wlong, Self>,
    ) -> bevy::ecs::query::QueryItem<'wshort, Self> {
        item
    }
}

unsafe impl<'a, Trait: ?Sized + TraitQuery> ReadOnlyWorldQuery for One<&'a Trait> {}

#[doc(hidden)]
pub struct ReadTraitFetch<'w, Trait: ?Sized> {
    // While we have shared access to all sparse set components,
    // in practice we will only read the components specified in the `FetchState`.
    // These accesses have been registered, which prevents runtime conflicts.
    sparse_sets: &'w SparseSets,
    // After `Fetch::set_archetype` or `set_table` has been called,
    // this will carry the component data and metadata for the first trait impl found in the archetype.
    storage: ReadStorage<'w, Trait>,
}

enum ReadStorage<'w, Trait: ?Sized> {
    Uninit,
    Table {
        /// This points to one of the component table columns,
        /// corresponding to one of the `ComponentId`s in the fetch state.
        /// The fetch impl registers read access for all of these components,
        /// so there will be no runtime conflicts.
        column: Ptr<'w>,
        entity_rows: ThinSlicePtr<'w, usize>,
        meta: TraitImplMeta<Trait>,
    },
    SparseSet {
        /// This gives us access to one of the components implementing the trait.
        /// The fetch impl registers read access for all components implementing the trait,
        /// so there will not be any runtime conflicts.
        components: &'w ComponentSparseSet,
        entities: ThinSlicePtr<'w, Entity>,
        meta: TraitImplMeta<Trait>,
    },
}

/// SAFETY: We only access the components registered in `DynQueryState`.
/// This same set of components is used to match archetypes, and used to register world access.
unsafe impl<'w, Trait: ?Sized + TraitQuery> Fetch<'w> for ReadTraitFetch<'w, Trait> {
    type Item = &'w Trait;
    type State = OneQueryState<Trait>;

    unsafe fn init(
        world: &'w World,
        _state: &Self::State,
        _last_change_tick: u32,
        _change_tick: u32,
    ) -> Self {
        Self {
            storage: ReadStorage::Uninit,
            sparse_sets: &world.storages().sparse_sets,
        }
    }

    const IS_DENSE: bool = false;
    const IS_ARCHETYPAL: bool = false;

    unsafe fn set_archetype(
        &mut self,
        state: &Self::State,
        archetype: &'w bevy::ecs::archetype::Archetype,
        tables: &'w bevy::ecs::storage::Tables,
    ) {
        // Search for a registered trait impl that is present in the archetype.
        // We check the table components first since it is faster to retrieve data of this type.
        let table = &tables[archetype.table_id()];
        for (&component, &meta) in zip_exact(&*state.components, &*state.meta) {
            if let Some(column) = table.get_column(component) {
                self.storage = ReadStorage::Table {
                    column: column.get_data_ptr(),
                    entity_rows: archetype.entity_table_rows().into(),
                    meta,
                };
                return;
            }
        }
        for (&component, &meta) in zip_exact(&*state.components, &*state.meta) {
            if let Some(sparse_set) = self.sparse_sets.get(component) {
                self.storage = ReadStorage::SparseSet {
                    entities: archetype.entities().into(),
                    components: sparse_set,
                    meta,
                };
                return;
            }
        }
        // At least one of the components must be present in the table/sparse set.
        debug_unreachable()
    }

    unsafe fn archetype_fetch(&mut self, archetype_index: usize) -> Self::Item {
        match self.storage {
            // SAFETY: This function must have been called after `set_archetype`,
            // so we know that `self.storage` has been initialized.
            ReadStorage::Uninit => debug_unreachable(),
            ReadStorage::Table {
                column,
                entity_rows,
                meta,
            } => {
                let table_row = *entity_rows.get(archetype_index);
                let ptr = column.byte_add(table_row * meta.size_bytes);
                meta.dyn_ctor.cast(ptr)
            }
            ReadStorage::SparseSet {
                entities,
                components,
                meta,
            } => {
                let entity = *entities.get(archetype_index);
                let ptr = components
                    .get(entity)
                    .unwrap_or_else(|| debug_unreachable());
                meta.dyn_ctor.cast(ptr)
            }
        }
    }

    unsafe fn set_table(&mut self, state: &Self::State, table: &'w bevy::ecs::storage::Table) {
        // Search for a registered trait impl that is present in the table.
        for (&component, &meta) in std::iter::zip(&*state.components, &*state.meta) {
            if let Some(column) = table.get_column(component) {
                self.storage = ReadStorage::Table {
                    column: column.get_data_ptr(),
                    entity_rows: (&[][..]).into(),
                    meta,
                }
            }
        }
        // At least one of the components must be present in the table.
        debug_unreachable()
    }

    unsafe fn table_fetch(&mut self, table_row: usize) -> Self::Item {
        match self.storage {
            // SAFETY: This function must have been called after `set_table`,
            // so we know that `self.storage` has been initialized to the variant `ReadStorage::Table`.
            ReadStorage::Uninit | ReadStorage::SparseSet { .. } => debug_unreachable(),
            ReadStorage::Table {
                column,
                entity_rows: _,
                meta,
            } => {
                let ptr = column.byte_add(table_row * meta.size_bytes);
                meta.dyn_ctor.cast(ptr)
            }
        }
    }

    fn update_component_access(
        state: &Self::State,
        access: &mut bevy::ecs::query::FilteredAccess<ComponentId>,
    ) {
        for &component in &*state.components {
            assert!(
                !access.access().has_write(component),
                "&{} conflicts with a previous access in this query. Shared access cannot coincide with exclusive access.",
                    std::any::type_name::<Trait>(),
            );
            access.add_read(component);
        }
    }

    fn update_archetype_component_access(
        state: &Self::State,
        archetype: &bevy::ecs::archetype::Archetype,
        access: &mut bevy::ecs::query::Access<bevy::ecs::archetype::ArchetypeComponentId>,
    ) {
        for &component in &*state.components {
            if let Some(archetype_component_id) = archetype.get_archetype_component_id(component) {
                access.add_read(archetype_component_id);
            }
        }
    }
}

#[doc(hidden)]
pub struct WriteTraitFetch<'w, Trait: ?Sized> {
    // While we have shared mutable access to all sparse set components,
    // in practice we will only modify the components specified in the `FetchState`.
    // These accesses have been registered, which prevents runtime conflicts.
    sparse_sets: &'w SparseSets,

    // After `Fetch::set_archetype` or `set_table` has been called,
    // this will carry the component data and metadata for the first trait impl found in the archetype.
    storage: WriteStorage<'w, Trait>,

    last_change_tick: u32,
    change_tick: u32,
}

enum WriteStorage<'w, Trait: ?Sized> {
    Uninit,
    Table {
        /// This is a shared mutable pointer to one of the component table columns,
        /// corresponding to one of the `ComponentId`s in the fetch state.
        /// The fetch impl registers write access for all of these components,
        /// so there will be no runtime conflicts.
        column: Ptr<'w>,
        table_ticks: ThinSlicePtr<'w, UnsafeCell<ComponentTicks>>,
        entity_rows: ThinSlicePtr<'w, usize>,
        meta: TraitImplMeta<Trait>,
    },
    SparseSet {
        /// This gives us shared mutable access to one of the components implementing the trait.
        /// The fetch impl registers write access for all components implementing the trait, so there will be no runtime conflicts.
        components: &'w ComponentSparseSet,
        entities: ThinSlicePtr<'w, Entity>,
        meta: TraitImplMeta<Trait>,
    },
}

impl<'w, 'a, Trait: ?Sized + TraitQuery> WorldQueryGats<'w> for One<&'a mut Trait> {
    type Fetch = WriteTraitFetch<'w, Trait>;
    type _State = OneQueryState<Trait>;
}

unsafe impl<'a, Trait: ?Sized + TraitQuery> WorldQuery for One<&'a mut Trait> {
    type ReadOnly = One<&'a Trait>;
    type State = OneQueryState<Trait>;

    fn shrink<'wlong: 'wshort, 'wshort>(
        item: bevy::ecs::query::QueryItem<'wlong, Self>,
    ) -> bevy::ecs::query::QueryItem<'wshort, Self> {
        item
    }
}

/// SAFETY: We only access the components registered in `DynQueryState`.
/// This same set of components is used to match archetypes, and used to register world access.
unsafe impl<'w, Trait: ?Sized + TraitQuery> Fetch<'w> for WriteTraitFetch<'w, Trait> {
    type Item = Mut<'w, Trait>;
    type State = OneQueryState<Trait>;

    unsafe fn init(
        world: &'w World,
        _state: &Self::State,
        last_change_tick: u32,
        change_tick: u32,
    ) -> Self {
        Self {
            storage: WriteStorage::Uninit,
            sparse_sets: &world.storages().sparse_sets,
            last_change_tick,
            change_tick,
        }
    }

    const IS_DENSE: bool = false;
    const IS_ARCHETYPAL: bool = false;

    unsafe fn set_archetype(
        &mut self,
        state: &Self::State,
        archetype: &'w bevy::ecs::archetype::Archetype,
        tables: &'w bevy::ecs::storage::Tables,
    ) {
        // Search for a registered trait impl that is present in the archetype.
        let table = &tables[archetype.table_id()];
        for (&component, &meta) in zip_exact(&*state.components, &*state.meta) {
            if let Some(column) = table.get_column(component) {
                self.storage = WriteStorage::Table {
                    column: column.get_data_ptr(),
                    table_ticks: column.get_ticks_slice().into(),
                    entity_rows: archetype.entity_table_rows().into(),
                    meta,
                };
                return;
            }
        }
        for (&component, &meta) in zip_exact(&*state.components, &*state.meta) {
            if let Some(sparse_set) = self.sparse_sets.get(component) {
                self.storage = WriteStorage::SparseSet {
                    entities: archetype.entities().into(),
                    components: sparse_set,
                    meta,
                };
                return;
            }
        }
        // At least one of the components must be present in the table/sparse set.
        debug_unreachable()
    }

    unsafe fn archetype_fetch(&mut self, archetype_index: usize) -> Self::Item {
        let dyn_ctor;
        let (ptr, component_ticks) = match self.storage {
            // SAFETY: This function must have been called after `set_archetype`,
            // so we know that `self.storage` has been initialized.
            WriteStorage::Uninit => debug_unreachable(),
            WriteStorage::Table {
                column,
                table_ticks,
                entity_rows,
                meta,
            } => {
                dyn_ctor = meta.dyn_ctor;
                let table_row = *entity_rows.get(archetype_index);
                let ptr = column.byte_add(table_row * meta.size_bytes);
                (
                    // SAFETY: `column` allows for shared mutable access.
                    // So long as the caller does not invoke this function twice with the same archetype_index,
                    // this pointer will never be aliased.
                    ptr.assert_unique(),
                    // SAFETY: We have exclusive access to the component, so by extension
                    // we have exclusive access to the corresponding `ComponentTicks`.
                    table_ticks.get(table_row).deref_mut(),
                )
            }
            WriteStorage::SparseSet {
                entities,
                components,
                meta,
            } => {
                dyn_ctor = meta.dyn_ctor;
                let entity = *entities.get(archetype_index);
                let (ptr, ticks) = components
                    .get_with_ticks(entity)
                    .unwrap_or_else(|| debug_unreachable());
                (
                    // SAFETY: We have exclusive access to the sparse set `components`.
                    // So long as the caller does not invoke this function twice with the same archetype_index,
                    // this pointer will never be aliased.
                    ptr.assert_unique(),
                    // SAFETY: We have exclusive access to the component, so by extension
                    // we have exclusive access to the corresponding `ComponentTicks`.
                    ticks.deref_mut(),
                )
            }
        };

        Mut {
            value: dyn_ctor.cast_mut(ptr),
            ticks: Ticks {
                component_ticks,
                last_change_tick: self.last_change_tick,
                change_tick: self.change_tick,
            },
        }
    }

    unsafe fn set_table(&mut self, state: &Self::State, table: &'w bevy::ecs::storage::Table) {
        // Search for a registered trait impl that is present in the table.
        for (&component, &meta) in std::iter::zip(&*state.components, &*state.meta) {
            if let Some(column) = table.get_column(component) {
                self.storage = WriteStorage::Table {
                    column: column.get_data_ptr(),
                    table_ticks: column.get_ticks_slice().into(),
                    entity_rows: [][..].into(),
                    meta,
                };
                return;
            }
        }
        // At least one of the components must be present in the table.
        debug_unreachable()
    }

    unsafe fn table_fetch(&mut self, table_row: usize) -> Self::Item {
        let (ptr, component_ticks, dyn_ctor) = match self.storage {
            // SAFETY: This function must have been called after `set_table`,
            // so we know that `self.storage` has been initialized to the variant `WriteStorage::Table`.
            WriteStorage::Uninit | WriteStorage::SparseSet { .. } => debug_unreachable(),
            WriteStorage::Table {
                column,
                table_ticks,
                entity_rows: _,
                meta,
            } => (
                column.byte_add(table_row * meta.size_bytes),
                table_ticks.get(table_row).deref_mut(),
                meta.dyn_ctor,
            ),
        };
        Mut {
            // Is `assert_unique` correct here??
            value: dyn_ctor.cast_mut(ptr.assert_unique()),
            ticks: Ticks {
                component_ticks,
                last_change_tick: self.last_change_tick,
                change_tick: self.change_tick,
            },
        }
    }

    fn update_component_access(
        state: &Self::State,
        access: &mut bevy::ecs::query::FilteredAccess<ComponentId>,
    ) {
        for &component in &*state.components {
            assert!(
                !access.access().has_write(component),
                "&mut {} conflicts with a previous access in this query. Mutable component access must be unique.",
                    std::any::type_name::<Trait>(),
            );
            access.add_write(component);
        }
    }

    fn update_archetype_component_access(
        state: &Self::State,
        archetype: &bevy::ecs::archetype::Archetype,
        access: &mut bevy::ecs::query::Access<bevy::ecs::archetype::ArchetypeComponentId>,
    ) {
        for &component in &*state.components {
            if let Some(archetype_component_id) = archetype.get_archetype_component_id(component) {
                access.add_write(archetype_component_id);
            }
        }
    }
}

/// `WorldQuery` adapter that fetches all implementations of a given trait for an entity.
///
/// You can usually just use `&dyn Trait` or `&mut dyn Trait` as a `WorldQuery` directly.
pub struct All<T: ?Sized>(T);

/// Read-access to all components implementing a trait for a given entity.
pub struct ReadTraits<'a, Trait: ?Sized + TraitQuery> {
    // Read-only access to the global trait registry.
    // Since no one outside of the crate can name the registry type,
    // we can be confident that no write accessess will conflict with this.
    registry: &'a TraitImplRegistry<Trait>,

    table: &'a Table,
    table_row: usize,

    /// This grants shared access to all sparse set components,
    /// but in practice we will only read the components specified in `self.registry`.
    /// The fetch impl registers read-access for all of these components,
    /// so there will be no runtime conflicts.
    sparse_sets: &'a SparseSets,
}

/// Write-access to all components implementing a trait for a given entity.
pub struct WriteTraits<'a, Trait: ?Sized + TraitQuery> {
    // Read-only access to the global trait registry.
    // Since no one outside of the crate can name the registry type,
    // we can be confident that no write accessess will conflict with this.
    registry: &'a TraitImplRegistry<Trait>,

    table: &'a Table,
    table_row: usize,

    last_change_tick: u32,
    change_tick: u32,

    /// This grants shared mutable access to all sparse set components,
    /// but in practice we will only modify the components specified in `self.registry`.
    /// The fetch impl registers write-access for all of these components,
    /// guaranteeing us exclusive access at runtime.
    sparse_sets: &'a SparseSets,
}

#[doc(hidden)]
pub type CombinedReadTraitsIter<'a, Trait> =
    std::iter::Chain<ReadTableTraitsIter<'a, Trait>, ReadSparseTraitsIter<'a, Trait>>;

#[doc(hidden)]
pub type CombinedWriteTraitsIter<'a, Trait> =
    std::iter::Chain<WriteTableTraitsIter<'a, Trait>, WriteSparseTraitsIter<'a, Trait>>;

#[doc(hidden)]
pub struct ReadTableTraitsIter<'a, Trait: ?Sized> {
    // SAFETY: These two iterators must have equal length.
    components: std::slice::Iter<'a, ComponentId>,
    meta: std::slice::Iter<'a, TraitImplMeta<Trait>>,
    table_row: usize,
    // Grants shared access to the components corresponding to `components` in this table.
    // Not all components are guaranteed to exist in the table.
    table: &'a Table,
}

impl<'a, Trait: ?Sized + TraitQuery> Iterator for ReadTableTraitsIter<'a, Trait> {
    type Item = &'a Trait;
    fn next(&mut self) -> Option<Self::Item> {
        // Iterate the remaining table components that are registered,
        // until we find one that exists in the table.
        let (column, meta) = unsafe { zip_exact(&mut self.components, &mut self.meta) }
            .find_map(|(&component, meta)| self.table.get_column(component).zip(Some(meta)))?;
        // SAFETY: We have shared access to the entire column.
        let ptr = unsafe {
            column
                .get_data_ptr()
                .byte_add(self.table_row * meta.size_bytes)
        };
        let trait_object = unsafe { meta.dyn_ctor.cast(ptr) };
        Some(trait_object)
    }
}

#[doc(hidden)]
pub struct ReadSparseTraitsIter<'a, Trait: ?Sized> {
    // SAFETY: These two iterators must have equal length.
    components: std::slice::Iter<'a, ComponentId>,
    meta: std::slice::Iter<'a, TraitImplMeta<Trait>>,
    entity: Entity,
    // Grants shared access to the components corresponding to both `components` and `entity`.
    sparse_sets: &'a SparseSets,
}

impl<'a, Trait: ?Sized + TraitQuery> Iterator for ReadSparseTraitsIter<'a, Trait> {
    type Item = &'a Trait;
    fn next(&mut self) -> Option<Self::Item> {
        // Iterate the remaining sparse set components that are registered,
        // until we find one that exists in the archetype.
        let (ptr, meta) = unsafe { zip_exact(&mut self.components, &mut self.meta) }.find_map(
            |(&component, meta)| {
                self.sparse_sets
                    .get(component)
                    .and_then(|set| set.get(self.entity))
                    .zip(Some(meta))
            },
        )?;
        let trait_object = unsafe { meta.dyn_ctor.cast(ptr) };
        Some(trait_object)
    }
}

#[doc(hidden)]
pub struct WriteTableTraitsIter<'a, Trait: ?Sized> {
    // SAFETY: These two iterators must have equal length.
    components: std::slice::Iter<'a, ComponentId>,
    meta: std::slice::Iter<'a, TraitImplMeta<Trait>>,
    table: &'a Table,
    /// SAFETY: Given the same trait type and same archetype,
    /// no two instances of this struct may have the same `table_row`.
    table_row: usize,
    last_change_tick: u32,
    change_tick: u32,
}

impl<'a, Trait: ?Sized + TraitQuery> Iterator for WriteTableTraitsIter<'a, Trait> {
    type Item = Mut<'a, Trait>;
    fn next(&mut self) -> Option<Self::Item> {
        // Iterate the remaining table components that are registered,
        // until we find one that exists in the table.
        let (column, meta) = unsafe { zip_exact(&mut self.components, &mut self.meta) }
            .find_map(|(&component, meta)| self.table.get_column(component).zip(Some(meta)))?;
        let ptr = unsafe {
            column
                .get_data_ptr()
                .byte_add(self.table_row * meta.size_bytes)
        };
        // SAFETY: The instance of `WriteTraits` that created this iterator
        // has exclusive access to all table components registered with the trait.
        //
        // Since `self.table_row` is guaranteed to be unique, we know that other instances
        // of `WriteTableTraitsIter` will not conflict with this pointer.
        let ptr = unsafe { ptr.assert_unique() };
        let trait_object = unsafe { meta.dyn_ctor.cast_mut(ptr) };
        // SAFETY: We have exclusive access to the component, so by extension
        // we have exclusive access to the corresponding `ComponentTicks`.
        let component_ticks = unsafe { column.get_ticks_unchecked(self.table_row).deref_mut() };
        Some(Mut {
            value: trait_object,
            ticks: Ticks {
                component_ticks,
                last_change_tick: self.last_change_tick,
                change_tick: self.change_tick,
            },
        })
    }
}

#[doc(hidden)]
pub struct WriteSparseTraitsIter<'a, Trait: ?Sized> {
    // SAFETY: These two iterators must have equal length.
    components: std::slice::Iter<'a, ComponentId>,
    meta: std::slice::Iter<'a, TraitImplMeta<Trait>>,
    /// SAFETY: Given the same trait type and same archetype,
    /// no two instances of this struct may have the same `entity`.
    entity: Entity,
    sparse_sets: &'a SparseSets,
    last_change_tick: u32,
    change_tick: u32,
}

impl<'a, Trait: ?Sized + TraitQuery> Iterator for WriteSparseTraitsIter<'a, Trait> {
    type Item = Mut<'a, Trait>;
    fn next(&mut self) -> Option<Self::Item> {
        // Iterate the remaining sparse set components we have registered,
        // until we find one that exists in the archetype.
        let ((ptr, component_ticks), meta) =
            unsafe { zip_exact(&mut self.components, &mut self.meta) }.find_map(
                |(&component, meta)| {
                    self.sparse_sets
                        .get(component)
                        .and_then(|set| set.get_with_ticks(self.entity))
                        .zip(Some(meta))
                },
            )?;

        // SAFETY: The instance of `WriteTraits` that created this iterator
        // has exclusive access to all sparse set components registered with the trait.
        //
        // Since `self.entity` is guaranteed to be unique, we know that other instances
        // of `WriteSparseTraitsIter` will not conflict with this pointer.
        let ptr = unsafe { ptr.assert_unique() };
        let trait_object = unsafe { meta.dyn_ctor.cast_mut(ptr) };
        // SAFETY: We have exclusive access to the component, so by extension
        // we have exclusive access to the corresponding `ComponentTicks`.
        let component_ticks = unsafe { component_ticks.deref_mut() };

        Some(Mut {
            value: trait_object,
            ticks: Ticks {
                component_ticks,
                last_change_tick: self.last_change_tick,
                change_tick: self.change_tick,
            },
        })
    }
}

#[doc(hidden)]
pub struct AllQueryState<Trait: ?Sized> {
    components: Box<[ComponentId]>,
    _marker: PhantomData<TraitImplMeta<Trait>>,
}

impl<Trait: ?Sized + TraitQuery> FetchState for AllQueryState<Trait> {
    fn init(world: &mut World) -> Self {
        #[cold]
        fn error<T: ?Sized + 'static>() -> ! {
            panic!(
                "no components found matching `{}`, did you forget to register them?",
                std::any::type_name::<T>()
            )
        }

        let mut registry = world
            .get_resource_mut::<TraitImplRegistry<Trait>>()
            .unwrap_or_else(|| error::<Trait>());
        registry.seal();
        Self {
            components: registry.components.clone().into_boxed_slice(),
            _marker: PhantomData,
        }
    }
    fn matches_component_set(&self, set_contains_id: &impl Fn(ComponentId) -> bool) -> bool {
        self.components.iter().copied().any(set_contains_id)
    }
}

#[doc(hidden)]
pub struct ReadAllTraitsFetch<'w, Trait: ?Sized> {
    registry: &'w TraitImplRegistry<Trait>,
    entity_table_rows: Option<ThinSlicePtr<'w, usize>>,
    table: Option<&'w Table>,
    sparse_sets: &'w SparseSets,
}

#[doc(hidden)]
pub struct WriteAllTraitsFetch<'w, Trait: ?Sized + TraitQuery> {
    registry: &'w TraitImplRegistry<Trait>,
    entity_table_rows: Option<ThinSlicePtr<'w, usize>>,
    table: Option<&'w Table>,
    sparse_sets: &'w SparseSets,

    last_change_tick: u32,
    change_tick: u32,
}

unsafe impl<'w, Trait: ?Sized + TraitQuery> WorldQuery for All<&'w Trait> {
    type ReadOnly = Self;
    type State = AllQueryState<Trait>;

    fn shrink<'wlong: 'wshort, 'wshort>(
        item: bevy::ecs::query::QueryItem<'wlong, Self>,
    ) -> bevy::ecs::query::QueryItem<'wshort, Self> {
        item
    }
}

unsafe impl<Trait: ?Sized + TraitQuery> ReadOnlyWorldQuery for All<&Trait> {}

impl<'w, Trait: ?Sized + TraitQuery> WorldQueryGats<'w> for All<&Trait> {
    type Fetch = ReadAllTraitsFetch<'w, Trait>;
    type _State = AllQueryState<Trait>;
}

unsafe impl<'w, Trait: ?Sized + TraitQuery> WorldQuery for All<&'w mut Trait> {
    type ReadOnly = All<&'w Trait>;
    type State = AllQueryState<Trait>;

    fn shrink<'wlong: 'wshort, 'wshort>(
        item: bevy::ecs::query::QueryItem<'wlong, Self>,
    ) -> bevy::ecs::query::QueryItem<'wshort, Self> {
        item
    }
}

impl<'w, Trait: ?Sized + TraitQuery> WorldQueryGats<'w> for All<&mut Trait> {
    type Fetch = WriteAllTraitsFetch<'w, Trait>;
    type _State = AllQueryState<Trait>;
}

/// SAFETY: We only access the components registered in the trait registry.
/// This is known to match the set of components in the `DynQueryState`,
/// which is used to match archetypes and register world access.
unsafe impl<'w, Trait: ?Sized + TraitQuery> Fetch<'w> for ReadAllTraitsFetch<'w, Trait> {
    type Item = ReadTraits<'w, Trait>;
    type State = AllQueryState<Trait>;

    unsafe fn init(
        world: &'w World,
        _state: &Self::State,
        _last_change_tick: u32,
        _change_tick: u32,
    ) -> Self {
        Self {
            entity_table_rows: None,
            registry: world.resource(),
            table: None,
            sparse_sets: &world.storages().sparse_sets,
        }
    }

    const IS_DENSE: bool = false;
    const IS_ARCHETYPAL: bool = false;

    unsafe fn set_archetype(
        &mut self,
        _state: &Self::State,
        archetype: &'w bevy::ecs::archetype::Archetype,
        tables: &'w bevy::ecs::storage::Tables,
    ) {
        self.entity_table_rows = Some(archetype.entity_table_rows().into());
        self.table = Some(&tables[archetype.table_id()]);
    }

    unsafe fn archetype_fetch(&mut self, archetype_index: usize) -> Self::Item {
        let entity_table_rows = self
            .entity_table_rows
            .unwrap_or_else(|| debug_unreachable());
        let table_row = *entity_table_rows.get(archetype_index);
        let table = self.table.unwrap_or_else(|| debug_unreachable());

        ReadTraits {
            registry: self.registry,
            table,
            table_row,
            sparse_sets: self.sparse_sets,
        }
    }

    unsafe fn set_table(&mut self, _state: &Self::State, table: &'w bevy::ecs::storage::Table) {
        self.table = Some(table);
    }

    unsafe fn table_fetch(&mut self, table_row: usize) -> Self::Item {
        let table = self.table.unwrap_or_else(|| debug_unreachable());

        ReadTraits {
            registry: self.registry,
            table,
            table_row,
            sparse_sets: self.sparse_sets,
        }
    }

    fn update_component_access(
        state: &Self::State,
        access: &mut bevy::ecs::query::FilteredAccess<ComponentId>,
    ) {
        for &component in &*state.components {
            assert!(
                !access.access().has_write(component),
                "&{} conflicts with a previous access in this query. Shared access cannot coincide with exclusive access.",
                    std::any::type_name::<Trait>(),
            );
            access.add_read(component);
        }
    }

    fn update_archetype_component_access(
        state: &Self::State,
        archetype: &bevy::ecs::archetype::Archetype,
        access: &mut bevy::ecs::query::Access<bevy::ecs::archetype::ArchetypeComponentId>,
    ) {
        for &component in &*state.components {
            if let Some(archetype_component_id) = archetype.get_archetype_component_id(component) {
                access.add_read(archetype_component_id);
            }
        }
    }
}

/// SAFETY: We only access the components registered in the trait registry.
/// This is known to match the set of components in the `DynQueryState`,
/// which is used to match archetypes and register world access.
unsafe impl<'w, Trait: ?Sized + TraitQuery> Fetch<'w> for WriteAllTraitsFetch<'w, Trait> {
    type Item = WriteTraits<'w, Trait>;
    type State = AllQueryState<Trait>;

    unsafe fn init(
        world: &'w World,
        _state: &Self::State,
        last_change_tick: u32,
        change_tick: u32,
    ) -> Self {
        Self {
            entity_table_rows: None,
            registry: world.resource(),
            table: None,
            sparse_sets: &world.storages().sparse_sets,
            last_change_tick,
            change_tick,
        }
    }

    const IS_DENSE: bool = false;
    const IS_ARCHETYPAL: bool = false;

    unsafe fn set_archetype(
        &mut self,
        _state: &Self::State,
        archetype: &'w bevy::ecs::archetype::Archetype,
        tables: &'w bevy::ecs::storage::Tables,
    ) {
        self.entity_table_rows = Some(archetype.entity_table_rows().into());
        self.table = Some(&tables[archetype.table_id()]);
    }

    unsafe fn archetype_fetch(&mut self, archetype_index: usize) -> Self::Item {
        let entity_table_rows = self
            .entity_table_rows
            .unwrap_or_else(|| debug_unreachable());
        let table_row = *entity_table_rows.get(archetype_index);
        let table = self.table.unwrap_or_else(|| debug_unreachable());

        WriteTraits {
            registry: self.registry,
            table,
            table_row,
            sparse_sets: self.sparse_sets,
            last_change_tick: self.last_change_tick,
            change_tick: self.change_tick,
        }
    }

    unsafe fn set_table(&mut self, _state: &Self::State, table: &'w bevy::ecs::storage::Table) {
        self.table = Some(table);
    }

    unsafe fn table_fetch(&mut self, table_row: usize) -> Self::Item {
        let table = self.table.unwrap_or_else(|| debug_unreachable());

        WriteTraits {
            registry: self.registry,
            table,
            table_row,
            sparse_sets: self.sparse_sets,
            last_change_tick: self.last_change_tick,
            change_tick: self.change_tick,
        }
    }

    fn update_component_access(
        state: &Self::State,
        access: &mut bevy::ecs::query::FilteredAccess<ComponentId>,
    ) {
        for &component in &*state.components {
            assert!(
                !access.access().has_write(component),
                "&mut {} conflicts with a previous access in this query. Mutable component access must be unique.",
                    std::any::type_name::<Trait>(),
            );
            access.add_write(component);
        }
    }

    fn update_archetype_component_access(
        state: &Self::State,
        archetype: &bevy::ecs::archetype::Archetype,
        access: &mut bevy::ecs::query::Access<bevy::ecs::archetype::ArchetypeComponentId>,
    ) {
        for &component in &*state.components {
            if let Some(archetype_component_id) = archetype.get_archetype_component_id(component) {
                access.add_write(archetype_component_id);
            }
        }
    }
}

impl<'w, Trait: ?Sized + TraitQuery> IntoIterator for ReadTraits<'w, Trait> {
    type Item = &'w Trait;
    type IntoIter = CombinedReadTraitsIter<'w, Trait>;
    fn into_iter(self) -> Self::IntoIter {
        let table = ReadTableTraitsIter {
            components: self.registry.table_components.iter(),
            meta: self.registry.table_meta.iter(),
            table: self.table,
            table_row: self.table_row,
        };
        let sparse = ReadSparseTraitsIter {
            components: self.registry.sparse_components.iter(),
            meta: self.registry.sparse_meta.iter(),
            entity: self.table.entities()[self.table_row],
            sparse_sets: self.sparse_sets,
        };
        table.chain(sparse)
    }
}

impl<'w, Trait: ?Sized + TraitQuery> IntoIterator for &ReadTraits<'w, Trait> {
    type Item = &'w Trait;
    type IntoIter = CombinedReadTraitsIter<'w, Trait>;
    fn into_iter(self) -> Self::IntoIter {
        let table = ReadTableTraitsIter {
            components: self.registry.table_components.iter(),
            meta: self.registry.table_meta.iter(),
            table: self.table,
            table_row: self.table_row,
        };
        let sparse = ReadSparseTraitsIter {
            components: self.registry.sparse_components.iter(),
            meta: self.registry.sparse_meta.iter(),
            entity: self.table.entities()[self.table_row],
            sparse_sets: self.sparse_sets,
        };
        table.chain(sparse)
    }
}

impl<'w, Trait: ?Sized + TraitQuery> IntoIterator for WriteTraits<'w, Trait> {
    type Item = Mut<'w, Trait>;
    type IntoIter = CombinedWriteTraitsIter<'w, Trait>;
    fn into_iter(self) -> Self::IntoIter {
        let table = WriteTableTraitsIter {
            components: self.registry.table_components.iter(),
            meta: self.registry.table_meta.iter(),
            table: self.table,
            table_row: self.table_row,
            last_change_tick: self.last_change_tick,
            change_tick: self.change_tick,
        };
        let sparse = WriteSparseTraitsIter {
            components: self.registry.sparse_components.iter(),
            meta: self.registry.sparse_meta.iter(),
            entity: self.table.entities()[self.table_row],
            sparse_sets: self.sparse_sets,
            last_change_tick: self.last_change_tick,
            change_tick: self.change_tick,
        };
        table.chain(sparse)
    }
}

impl<'world, 'local, Trait: ?Sized + TraitQuery> IntoIterator
    for &'local WriteTraits<'world, Trait>
{
    type Item = &'local Trait;
    type IntoIter = CombinedReadTraitsIter<'local, Trait>;
    fn into_iter(self) -> Self::IntoIter {
        let table = ReadTableTraitsIter {
            components: self.registry.table_components.iter(),
            meta: self.registry.table_meta.iter(),
            table: self.table,
            table_row: self.table_row,
        };
        let sparse = ReadSparseTraitsIter {
            components: self.registry.sparse_components.iter(),
            meta: self.registry.sparse_meta.iter(),
            entity: self.table.entities()[self.table_row],
            sparse_sets: self.sparse_sets,
        };
        table.chain(sparse)
    }
}

impl<'world, 'local, Trait: ?Sized + TraitQuery> IntoIterator
    for &'local mut WriteTraits<'world, Trait>
{
    type Item = Mut<'local, Trait>;
    type IntoIter = CombinedWriteTraitsIter<'local, Trait>;
    fn into_iter(self) -> Self::IntoIter {
        let table = WriteTableTraitsIter {
            components: self.registry.table_components.iter(),
            meta: self.registry.table_meta.iter(),
            table: self.table,
            table_row: self.table_row,
            last_change_tick: self.last_change_tick,
            change_tick: self.change_tick,
        };
        let sparse = WriteSparseTraitsIter {
            components: self.registry.sparse_components.iter(),
            meta: self.registry.sparse_meta.iter(),
            entity: self.table.entities()[self.table_row],
            sparse_sets: self.sparse_sets,
            last_change_tick: self.last_change_tick,
            change_tick: self.change_tick,
        };
        table.chain(sparse)
    }
}

#[track_caller]
#[inline(always)]
unsafe fn debug_unreachable() -> ! {
    #[cfg(debug_assertions)]
    unreachable!();

    #[cfg(not(debug_assertions))]
    std::hint::unreachable_unchecked();
}
