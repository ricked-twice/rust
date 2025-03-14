//! The implementation of the query system itself. This defines the macros that
//! generate the actual methods on tcx which find and execute the provider,
//! manage the caches, and so forth.

use crate::keys::Key;
use crate::{on_disk_cache, Queries};
use rustc_middle::dep_graph::{self, DepKind, DepNodeIndex, SerializedDepNodeIndex};
use rustc_middle::ty::tls::{self, ImplicitCtxt};
use rustc_middle::ty::{self, TyCtxt};
use rustc_query_system::dep_graph::HasDepContext;
use rustc_query_system::ich::StableHashingContext;
use rustc_query_system::query::{
    QueryContext, QueryJobId, QueryMap, QuerySideEffects, QueryStackFrame,
};

use rustc_data_structures::stable_hasher::{HashStable, StableHasher};
use rustc_data_structures::sync::Lock;
use rustc_data_structures::thin_vec::ThinVec;
use rustc_errors::{Diagnostic, Handler};

use std::any::Any;
use std::num::NonZeroU64;

#[derive(Copy, Clone)]
pub struct QueryCtxt<'tcx> {
    pub tcx: TyCtxt<'tcx>,
    pub queries: &'tcx Queries<'tcx>,
}

impl<'tcx> std::ops::Deref for QueryCtxt<'tcx> {
    type Target = TyCtxt<'tcx>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.tcx
    }
}

impl<'tcx> HasDepContext for QueryCtxt<'tcx> {
    type DepKind = rustc_middle::dep_graph::DepKind;
    type DepContext = TyCtxt<'tcx>;

    #[inline]
    fn dep_context(&self) -> &Self::DepContext {
        &self.tcx
    }
}

impl QueryContext for QueryCtxt<'_> {
    fn next_job_id(&self) -> QueryJobId {
        QueryJobId(
            NonZeroU64::new(
                self.queries.jobs.fetch_add(1, rustc_data_structures::sync::Ordering::Relaxed),
            )
            .unwrap(),
        )
    }

    fn current_query_job(&self) -> Option<QueryJobId> {
        tls::with_related_context(**self, |icx| icx.query)
    }

    fn try_collect_active_jobs(&self) -> Option<QueryMap> {
        self.queries.try_collect_active_jobs(**self)
    }

    // Interactions with on_disk_cache
    fn load_side_effects(&self, prev_dep_node_index: SerializedDepNodeIndex) -> QuerySideEffects {
        self.queries
            .on_disk_cache
            .as_ref()
            .map(|c| c.load_side_effects(**self, prev_dep_node_index))
            .unwrap_or_default()
    }

    fn store_side_effects(&self, dep_node_index: DepNodeIndex, side_effects: QuerySideEffects) {
        if let Some(c) = self.queries.on_disk_cache.as_ref() {
            c.store_side_effects(dep_node_index, side_effects)
        }
    }

    fn store_side_effects_for_anon_node(
        &self,
        dep_node_index: DepNodeIndex,
        side_effects: QuerySideEffects,
    ) {
        if let Some(c) = self.queries.on_disk_cache.as_ref() {
            c.store_side_effects_for_anon_node(dep_node_index, side_effects)
        }
    }

    /// Executes a job by changing the `ImplicitCtxt` to point to the
    /// new query job while it executes. It returns the diagnostics
    /// captured during execution and the actual result.
    #[inline(always)]
    fn start_query<R>(
        &self,
        token: QueryJobId,
        depth_limit: bool,
        diagnostics: Option<&Lock<ThinVec<Diagnostic>>>,
        compute: impl FnOnce() -> R,
    ) -> R {
        // The `TyCtxt` stored in TLS has the same global interner lifetime
        // as `self`, so we use `with_related_context` to relate the 'tcx lifetimes
        // when accessing the `ImplicitCtxt`.
        tls::with_related_context(**self, move |current_icx| {
            if depth_limit && !self.recursion_limit().value_within_limit(current_icx.query_depth) {
                self.depth_limit_error();
            }

            // Update the `ImplicitCtxt` to point to our new query job.
            let new_icx = ImplicitCtxt {
                tcx: **self,
                query: Some(token),
                diagnostics,
                query_depth: current_icx.query_depth + depth_limit as usize,
                task_deps: current_icx.task_deps,
            };

            // Use the `ImplicitCtxt` while we execute the query.
            tls::enter_context(&new_icx, |_| {
                rustc_data_structures::stack::ensure_sufficient_stack(compute)
            })
        })
    }
}

impl<'tcx> QueryCtxt<'tcx> {
    #[inline]
    pub fn from_tcx(tcx: TyCtxt<'tcx>) -> Self {
        let queries = tcx.queries.as_any();
        let queries = unsafe {
            let queries = std::mem::transmute::<&dyn Any, &dyn Any>(queries);
            let queries = queries.downcast_ref().unwrap();
            let queries = std::mem::transmute::<&Queries<'_>, &Queries<'_>>(queries);
            queries
        };
        QueryCtxt { tcx, queries }
    }

    pub(crate) fn on_disk_cache(self) -> Option<&'tcx on_disk_cache::OnDiskCache<'tcx>> {
        self.queries.on_disk_cache.as_ref()
    }

    pub(super) fn encode_query_results(
        self,
        encoder: &mut on_disk_cache::CacheEncoder<'_, 'tcx>,
        query_result_index: &mut on_disk_cache::EncodedDepNodeIndex,
    ) {
        macro_rules! encode_queries {
            ($($query:ident,)*) => {
                $(
                    on_disk_cache::encode_query_results::<_, super::queries::$query<'_>>(
                        self,
                        encoder,
                        query_result_index
                    );
                )*
            }
        }

        rustc_cached_queries!(encode_queries!);
    }

    pub fn try_print_query_stack(
        self,
        query: Option<QueryJobId>,
        handler: &Handler,
        num_frames: Option<usize>,
    ) -> usize {
        rustc_query_system::query::print_query_stack(self, query, handler, num_frames)
    }
}

macro_rules! handle_cycle_error {
    ([][$tcx: expr, $error:expr]) => {{
        $error.emit();
        Value::from_cycle_error($tcx)
    }};
    ([(fatal_cycle) $($rest:tt)*][$tcx:expr, $error:expr]) => {{
        $error.emit();
        $tcx.sess.abort_if_errors();
        unreachable!()
    }};
    ([(cycle_delay_bug) $($rest:tt)*][$tcx:expr, $error:expr]) => {{
        $error.delay_as_bug();
        Value::from_cycle_error($tcx)
    }};
    ([$other:tt $($modifiers:tt)*][$($args:tt)*]) => {
        handle_cycle_error!([$($modifiers)*][$($args)*])
    };
}

macro_rules! is_anon {
    ([]) => {{
        false
    }};
    ([(anon) $($rest:tt)*]) => {{
        true
    }};
    ([$other:tt $($modifiers:tt)*]) => {
        is_anon!([$($modifiers)*])
    };
}

macro_rules! is_eval_always {
    ([]) => {{
        false
    }};
    ([(eval_always) $($rest:tt)*]) => {{
        true
    }};
    ([$other:tt $($modifiers:tt)*]) => {
        is_eval_always!([$($modifiers)*])
    };
}

macro_rules! depth_limit {
    ([]) => {{
        false
    }};
    ([(depth_limit) $($rest:tt)*]) => {{
        true
    }};
    ([$other:tt $($modifiers:tt)*]) => {
        depth_limit!([$($modifiers)*])
    };
}

macro_rules! hash_result {
    ([]) => {{
        Some(dep_graph::hash_result)
    }};
    ([(no_hash) $($rest:tt)*]) => {{
        None
    }};
    ([$other:tt $($modifiers:tt)*]) => {
        hash_result!([$($modifiers)*])
    };
}

macro_rules! get_provider {
    ([][$tcx:expr, $name:ident, $key:expr]) => {{
        $tcx.queries.local_providers.$name
    }};
    ([(separate_provide_extern) $($rest:tt)*][$tcx:expr, $name:ident, $key:expr]) => {{
        if $key.query_crate_is_local() {
            $tcx.queries.local_providers.$name
        } else {
            $tcx.queries.extern_providers.$name
        }
    }};
    ([$other:tt $($modifiers:tt)*][$($args:tt)*]) => {
        get_provider!([$($modifiers)*][$($args)*])
    };
}

pub(crate) fn create_query_frame<
    'tcx,
    K: Copy + Key + for<'a> HashStable<StableHashingContext<'a>>,
>(
    tcx: QueryCtxt<'tcx>,
    do_describe: fn(QueryCtxt<'tcx>, K) -> String,
    key: K,
    kind: DepKind,
    name: &'static str,
) -> QueryStackFrame {
    // Disable visible paths printing for performance reasons.
    // Showing visible path instead of any path is not that important in production.
    let description = ty::print::with_no_visible_paths!(
        // Force filename-line mode to avoid invoking `type_of` query.
        ty::print::with_forced_impl_filename_line!(do_describe(tcx, key))
    );
    let description =
        if tcx.sess.verbose() { format!("{} [{}]", description, name) } else { description };
    let span = if kind == dep_graph::DepKind::def_span {
        // The `def_span` query is used to calculate `default_span`,
        // so exit to avoid infinite recursion.
        None
    } else {
        Some(key.default_span(*tcx))
    };
    let def_kind = if kind == dep_graph::DepKind::opt_def_kind {
        // Try to avoid infinite recursion.
        None
    } else {
        key.key_as_def_id()
            .and_then(|def_id| def_id.as_local())
            .and_then(|def_id| tcx.opt_def_kind(def_id))
    };
    let hash = || {
        tcx.with_stable_hashing_context(|mut hcx| {
            let mut hasher = StableHasher::new();
            std::mem::discriminant(&kind).hash_stable(&mut hcx, &mut hasher);
            key.hash_stable(&mut hcx, &mut hasher);
            hasher.finish::<u64>()
        })
    };

    QueryStackFrame::new(name, description, span, def_kind, hash)
}

// NOTE: `$V` isn't used here, but we still need to match on it so it can be passed to other macros
// invoked by `rustc_query_append`.
macro_rules! define_queries {
    (
     $($(#[$attr:meta])*
        [$($modifiers:tt)*] fn $name:ident($($K:tt)*) -> $V:ty,)*) => {
        define_queries_struct! {
            input: ($(([$($modifiers)*] [$($attr)*] [$name]))*)
        }

        mod make_query {
            use super::*;

            // Create an eponymous constructor for each query.
            $(#[allow(nonstandard_style)] $(#[$attr])*
            pub fn $name<'tcx>(tcx: QueryCtxt<'tcx>, key: <queries::$name<'tcx> as QueryConfig>::Key) -> QueryStackFrame {
                let kind = dep_graph::DepKind::$name;
                let name = stringify!($name);
                $crate::plumbing::create_query_frame(tcx, queries::$name::describe, key, kind, name)
            })*
        }

        #[allow(nonstandard_style)]
        mod queries {
            use std::marker::PhantomData;

            $(pub struct $name<'tcx> {
                data: PhantomData<&'tcx ()>
            })*
        }

        $(impl<'tcx> QueryConfig for queries::$name<'tcx> {
            type Key = query_keys::$name<'tcx>;
            type Value = query_values::$name<'tcx>;
            type Stored = query_stored::$name<'tcx>;
            const NAME: &'static str = stringify!($name);
        }

        impl<'tcx> QueryDescription<QueryCtxt<'tcx>> for queries::$name<'tcx> {
            rustc_query_description! { $name<'tcx> }

            type Cache = query_storage::$name<'tcx>;

            #[inline(always)]
            fn query_state<'a>(tcx: QueryCtxt<'tcx>) -> &'a QueryState<Self::Key>
                where QueryCtxt<'tcx>: 'a
            {
                &tcx.queries.$name
            }

            #[inline(always)]
            fn query_cache<'a>(tcx: QueryCtxt<'tcx>) -> &'a Self::Cache
                where 'tcx:'a
            {
                &tcx.query_caches.$name
            }

            #[inline]
            fn make_vtable(tcx: QueryCtxt<'tcx>, key: &Self::Key) ->
                QueryVTable<QueryCtxt<'tcx>, Self::Key, Self::Value>
            {
                let compute = get_provider!([$($modifiers)*][tcx, $name, key]);
                let cache_on_disk = Self::cache_on_disk(tcx.tcx, key);
                QueryVTable {
                    anon: is_anon!([$($modifiers)*]),
                    eval_always: is_eval_always!([$($modifiers)*]),
                    depth_limit: depth_limit!([$($modifiers)*]),
                    dep_kind: dep_graph::DepKind::$name,
                    hash_result: hash_result!([$($modifiers)*]),
                    handle_cycle_error: |tcx, mut error| handle_cycle_error!([$($modifiers)*][tcx, error]),
                    compute,
                    cache_on_disk,
                    try_load_from_disk: Self::TRY_LOAD_FROM_DISK,
                }
            }
        })*

        #[allow(nonstandard_style)]
        mod query_callbacks {
            use super::*;
            use rustc_middle::dep_graph::DepNode;
            use rustc_query_system::dep_graph::DepNodeParams;
            use rustc_query_system::query::{force_query, QueryDescription};
            use rustc_query_system::dep_graph::FingerprintStyle;

            // We use this for most things when incr. comp. is turned off.
            pub fn Null() -> DepKindStruct {
                DepKindStruct {
                    is_anon: false,
                    is_eval_always: false,
                    fingerprint_style: FingerprintStyle::Unit,
                    force_from_dep_node: Some(|_, dep_node| bug!("force_from_dep_node: encountered {:?}", dep_node)),
                    try_load_from_on_disk_cache: None,
                }
            }

            // We use this for the forever-red node.
            pub fn Red() -> DepKindStruct {
                DepKindStruct {
                    is_anon: false,
                    is_eval_always: false,
                    fingerprint_style: FingerprintStyle::Unit,
                    force_from_dep_node: Some(|_, dep_node| bug!("force_from_dep_node: encountered {:?}", dep_node)),
                    try_load_from_on_disk_cache: None,
                }
            }

            pub fn TraitSelect() -> DepKindStruct {
                DepKindStruct {
                    is_anon: true,
                    is_eval_always: false,
                    fingerprint_style: FingerprintStyle::Unit,
                    force_from_dep_node: None,
                    try_load_from_on_disk_cache: None,
                }
            }

            pub fn CompileCodegenUnit() -> DepKindStruct {
                DepKindStruct {
                    is_anon: false,
                    is_eval_always: false,
                    fingerprint_style: FingerprintStyle::Opaque,
                    force_from_dep_node: None,
                    try_load_from_on_disk_cache: None,
                }
            }

            pub fn CompileMonoItem() -> DepKindStruct {
                DepKindStruct {
                    is_anon: false,
                    is_eval_always: false,
                    fingerprint_style: FingerprintStyle::Opaque,
                    force_from_dep_node: None,
                    try_load_from_on_disk_cache: None,
                }
            }

            $(pub(crate) fn $name()-> DepKindStruct {
                let is_anon = is_anon!([$($modifiers)*]);
                let is_eval_always = is_eval_always!([$($modifiers)*]);

                let fingerprint_style =
                    <<queries::$name<'_> as QueryConfig>::Key as DepNodeParams<TyCtxt<'_>>>::fingerprint_style();

                if is_anon || !fingerprint_style.reconstructible() {
                    return DepKindStruct {
                        is_anon,
                        is_eval_always,
                        fingerprint_style,
                        force_from_dep_node: None,
                        try_load_from_on_disk_cache: None,
                    }
                }

                #[inline(always)]
                fn recover<'tcx>(tcx: TyCtxt<'tcx>, dep_node: DepNode) -> Option<<queries::$name<'tcx> as QueryConfig>::Key> {
                    <<queries::$name<'_> as QueryConfig>::Key as DepNodeParams<TyCtxt<'_>>>::recover(tcx, &dep_node)
                }

                fn force_from_dep_node(tcx: TyCtxt<'_>, dep_node: DepNode) -> bool {
                    if let Some(key) = recover(tcx, dep_node) {
                        #[cfg(debug_assertions)]
                        let _guard = tracing::span!(tracing::Level::TRACE, stringify!($name), ?key).entered();
                        let tcx = QueryCtxt::from_tcx(tcx);
                        force_query::<queries::$name<'_>, _>(tcx, key, dep_node);
                        true
                    } else {
                        false
                    }
                }

                fn try_load_from_on_disk_cache(tcx: TyCtxt<'_>, dep_node: DepNode) {
                    debug_assert!(tcx.dep_graph.is_green(&dep_node));

                    let key = recover(tcx, dep_node).unwrap_or_else(|| panic!("Failed to recover key for {:?} with hash {}", dep_node, dep_node.hash));
                    if queries::$name::cache_on_disk(tcx, &key) {
                        let _ = tcx.$name(key);
                    }
                }

                DepKindStruct {
                    is_anon,
                    is_eval_always,
                    fingerprint_style,
                    force_from_dep_node: Some(force_from_dep_node),
                    try_load_from_on_disk_cache: Some(try_load_from_on_disk_cache),
                }
            })*
        }

        pub fn query_callbacks<'tcx>(arena: &'tcx Arena<'tcx>) -> &'tcx [DepKindStruct] {
            arena.alloc_from_iter(make_dep_kind_array!(query_callbacks))
        }
    }
}

macro_rules! define_queries_struct {
    (
     input: ($(([$($modifiers:tt)*] [$($attr:tt)*] [$name:ident]))*)) => {
        pub struct Queries<'tcx> {
            local_providers: Box<Providers>,
            extern_providers: Box<ExternProviders>,

            pub on_disk_cache: Option<OnDiskCache<'tcx>>,

            jobs: AtomicU64,

            $($(#[$attr])*  $name: QueryState<<queries::$name<'tcx> as QueryConfig>::Key>,)*
        }

        impl<'tcx> Queries<'tcx> {
            pub fn new(
                local_providers: Providers,
                extern_providers: ExternProviders,
                on_disk_cache: Option<OnDiskCache<'tcx>>,
            ) -> Self {
                Queries {
                    local_providers: Box::new(local_providers),
                    extern_providers: Box::new(extern_providers),
                    on_disk_cache,
                    jobs: AtomicU64::new(1),
                    $($name: Default::default()),*
                }
            }

            pub(crate) fn try_collect_active_jobs(
                &'tcx self,
                tcx: TyCtxt<'tcx>,
            ) -> Option<QueryMap> {
                let tcx = QueryCtxt { tcx, queries: self };
                let mut jobs = QueryMap::default();

                $(
                    self.$name.try_collect_active_jobs(
                        tcx,
                        make_query::$name,
                        &mut jobs,
                    )?;
                )*

                Some(jobs)
            }
        }

        impl<'tcx> QueryEngine<'tcx> for Queries<'tcx> {
            fn as_any(&'tcx self) -> &'tcx dyn std::any::Any {
                let this = unsafe { std::mem::transmute::<&Queries<'_>, &Queries<'_>>(self) };
                this as _
            }

            fn try_mark_green(&'tcx self, tcx: TyCtxt<'tcx>, dep_node: &dep_graph::DepNode) -> bool {
                let qcx = QueryCtxt { tcx, queries: self };
                tcx.dep_graph.try_mark_green(qcx, dep_node).is_some()
            }

            $($(#[$attr])*
            #[inline(always)]
            #[tracing::instrument(level = "trace", skip(self, tcx))]
            fn $name(
                &'tcx self,
                tcx: TyCtxt<'tcx>,
                span: Span,
                key: <queries::$name<'tcx> as QueryConfig>::Key,
                mode: QueryMode,
            ) -> Option<query_stored::$name<'tcx>> {
                let qcx = QueryCtxt { tcx, queries: self };
                get_query::<queries::$name<'tcx>, _>(qcx, span, key, mode)
            })*
        }
    };
}
