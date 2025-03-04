// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use differential_dataflow::difference::Semigroup;
use differential_dataflow::AsCollection;
use differential_dataflow::Collection;
use timely::dataflow::channels::pact::{ParallelizationContract, Pipeline};
use timely::dataflow::channels::pushers::Tee;
use timely::dataflow::operators::generic::builder_rc::OperatorBuilder;
use timely::dataflow::operators::generic::{
    operator::{self, Operator},
    InputHandle, OperatorInfo, OutputHandle,
};
use timely::dataflow::operators::Capability;
use timely::dataflow::{Scope, Stream};
use timely::Data;

use repr::Diff;

/// Extension methods for timely [`Stream`]s.
pub trait StreamExt<G, D1>
where
    D1: Data,
    G: Scope,
{
    /// Like `timely::dataflow::operators::generic::operator::Operator::unary`,
    /// but the logic function can handle failures.
    ///
    /// Creates a new dataflow operator that partitions its input stream by a
    /// parallelization strategy `pact` and repeatedly invokes `logic`, the
    /// function returned by the function passed as `constructor`. The `logic`
    /// function can read to the input stream and write to either of two output
    /// streams, where the first output stream represents successful
    /// computations and the second output stream represents failed
    /// computations.
    fn unary_fallible<D2, E, B, P>(
        &self,
        pact: P,
        name: &str,
        constructor: B,
    ) -> (Stream<G, D2>, Stream<G, E>)
    where
        D2: Data,
        E: Data,
        B: FnOnce(
            Capability<G::Timestamp>,
            OperatorInfo,
        ) -> Box<
            dyn FnMut(
                    &mut InputHandle<G::Timestamp, D1, P::Puller>,
                    &mut OutputHandle<G::Timestamp, D2, Tee<G::Timestamp, D2>>,
                    &mut OutputHandle<G::Timestamp, E, Tee<G::Timestamp, E>>,
                ) + 'static,
        >,
        P: ParallelizationContract<G::Timestamp, D1>;

    /// Like [`timely::dataflow::operators::map::Map::map`], but `logic`
    /// is allowed to fail. The first returned stream will contain the
    /// successful applications of `logic`, while the second returned stream
    /// will contain the failed applications.
    fn map_fallible<D2, E, L>(&self, name: &str, mut logic: L) -> (Stream<G, D2>, Stream<G, E>)
    where
        D2: Data,
        E: Data,
        L: FnMut(D1) -> Result<D2, E> + 'static,
    {
        self.flat_map_fallible(name, move |record| Some(logic(record)))
    }

    /// Like [`timely::dataflow::operators::map::Map::flat_map`], but `logic`
    /// is allowed to fail. The first returned stream will contain the
    /// successful applications of `logic`, while the second returned stream
    /// will contain the failed applications.
    fn flat_map_fallible<D2, E, I, L>(&self, name: &str, logic: L) -> (Stream<G, D2>, Stream<G, E>)
    where
        D2: Data,
        E: Data,
        I: IntoIterator<Item = Result<D2, E>>,
        L: FnMut(D1) -> I + 'static;

    /// Take a Timely stream and convert it to a Differential stream, where each diff is "1"
    /// and each time is the current Timely timestamp.
    fn pass_through(&self, name: &str) -> Stream<G, (D1, G::Timestamp, Diff)>;
}

/// Extension methods for differential [`Collection`]s.
pub trait CollectionExt<G, D1, R>
where
    G: Scope,
    R: Semigroup,
{
    /// Creates a new empty collection in `scope`.
    fn empty(scope: &G) -> Collection<G, D1, R>;

    /// Like [`Collection::map`], but `logic` is allowed to fail. The first
    /// returned collection will contain successful applications of `logic`,
    /// while the second returned collection will contain the failed
    /// applications.
    fn map_fallible<D2, E, L>(
        &self,
        name: &str,
        mut logic: L,
    ) -> (Collection<G, D2, R>, Collection<G, E, R>)
    where
        D2: Data,
        E: Data,
        L: FnMut(D1) -> Result<D2, E> + 'static,
    {
        self.flat_map_fallible(name, move |record| Some(logic(record)))
    }

    /// Like [`Collection::flat_map`], but `logic` is allowed to fail. The first
    /// returned collection will contain the successful applications of `logic`,
    /// while the second returned collection will contain the failed
    /// applications.
    fn flat_map_fallible<D2, E, I, L>(
        &self,
        name: &str,
        logic: L,
    ) -> (Collection<G, D2, R>, Collection<G, E, R>)
    where
        D2: Data,
        E: Data,
        I: IntoIterator<Item = Result<D2, E>>,
        L: FnMut(D1) -> I + 'static;
}

impl<G, D1> StreamExt<G, D1> for Stream<G, D1>
where
    D1: Data,
    G: Scope,
{
    fn unary_fallible<D2, E, B, P>(
        &self,
        pact: P,
        name: &str,
        constructor: B,
    ) -> (Stream<G, D2>, Stream<G, E>)
    where
        D2: Data,
        E: Data,
        B: FnOnce(
            Capability<G::Timestamp>,
            OperatorInfo,
        ) -> Box<
            dyn FnMut(
                    &mut InputHandle<G::Timestamp, D1, P::Puller>,
                    &mut OutputHandle<G::Timestamp, D2, Tee<G::Timestamp, D2>>,
                    &mut OutputHandle<G::Timestamp, E, Tee<G::Timestamp, E>>,
                ) + 'static,
        >,
        P: ParallelizationContract<G::Timestamp, D1>,
    {
        let mut builder = OperatorBuilder::new(name.into(), self.scope());
        builder.set_notify(false);

        let operator_info = builder.operator_info();

        let mut input = builder.new_input(self, pact);
        let (mut ok_output, ok_stream) = builder.new_output();
        let (mut err_output, err_stream) = builder.new_output();

        builder.build(move |mut capabilities| {
            // `capabilities` should be a single-element vector.
            let capability = capabilities.pop().unwrap();
            let mut logic = constructor(capability, operator_info);
            move |_frontiers| {
                let mut ok_output_handle = ok_output.activate();
                let mut err_output_handle = err_output.activate();
                logic(&mut input, &mut ok_output_handle, &mut err_output_handle);
            }
        });

        (ok_stream, err_stream)
    }

    // XXX(guswynn): file an minimization bug report for the logic flat_map
    // false positive here
    // TODO(guswynn): remove this after https://github.com/rust-lang/rust-clippy/issues/8098 is
    // resolved. The `logic` `FnMut` needs to be borrowed in the `flat_map` call, not moved in
    // so the simple `|d1| logic(d1)` closure is load-bearing
    #[allow(clippy::redundant_closure)]
    fn flat_map_fallible<D2, E, I, L>(
        &self,
        name: &str,
        mut logic: L,
    ) -> (Stream<G, D2>, Stream<G, E>)
    where
        D2: Data,
        E: Data,
        I: IntoIterator<Item = Result<D2, E>>,
        L: FnMut(D1) -> I + 'static,
    {
        let mut storage = Vec::new();
        self.unary_fallible(Pipeline, name, move |_, _| {
            Box::new(move |input, ok_output, err_output| {
                input.for_each(|time, data| {
                    let mut ok_session = ok_output.session(&time);
                    let mut err_session = err_output.session(&time);
                    data.swap(&mut storage);
                    for r in storage.drain(..).flat_map(|d1| logic(d1)) {
                        match r {
                            Ok(d2) => ok_session.give(d2),
                            Err(e) => err_session.give(e),
                        }
                    }
                })
            })
        })
    }

    fn pass_through(
        &self,
        name: &str,
    ) -> Stream<
        G,
        (
            D1,
            G::Timestamp,
            Diff, /* Can't be generic -- Semigroup has no distinguished `1` element */
        ),
    > {
        self.unary(Pipeline, name, move |_, _| {
            move |input, output| {
                input.for_each(|cap, data| {
                    let mut v = Vec::new();
                    data.swap(&mut v);
                    let mut session = output.session(&cap);
                    session.give_iterator(
                        v.into_iter()
                            .map(|payload| (payload, cap.time().clone(), 1)),
                    );
                });
            }
        })
    }
}

impl<G, D1, R> CollectionExt<G, D1, R> for Collection<G, D1, R>
where
    G: Scope,
    G::Timestamp: Data,
    D1: Data,
    R: Semigroup,
{
    fn empty(scope: &G) -> Collection<G, D1, R> {
        operator::empty(scope).as_collection()
    }

    fn flat_map_fallible<D2, E, I, L>(
        &self,
        name: &str,
        mut logic: L,
    ) -> (Collection<G, D2, R>, Collection<G, E, R>)
    where
        D2: Data,
        E: Data,
        I: IntoIterator<Item = Result<D2, E>>,
        L: FnMut(D1) -> I + 'static,
    {
        let (ok_stream, err_stream) = self.inner.flat_map_fallible(name, move |(d1, t, r)| {
            logic(d1).into_iter().map(move |res| match res {
                Ok(d2) => Ok((d2, t.clone(), r.clone())),
                Err(e) => Err((e, t.clone(), r.clone())),
            })
        });
        (ok_stream.as_collection(), err_stream.as_collection())
    }
}
