use super::{lookup, permutation, shuffle, Error, Queries};
use crate::circuit::layouter::SyncDeps;
use crate::circuit::{Layouter, Region, Value};
use crate::plonk::Assigned;
use core::cmp::max;
use core::ops::{Add, Mul};
use halo2_middleware::circuit::{
    Advice, AdviceQueryMid, Any, ChallengeMid, ColumnMid, ColumnType, ConstraintSystemV2Backend,
    ExpressionMid, Fixed, FixedQueryMid, GateV2Backend, Instance, InstanceQueryMid,
};
use halo2_middleware::ff::Field;
use halo2_middleware::metadata;
use halo2_middleware::poly::Rotation;
use sealed::SealedPhase;
use std::collections::HashMap;
use std::fmt::Debug;
use std::iter::{Product, Sum};
use std::{
    convert::TryFrom,
    ops::{Neg, Sub},
};

mod compress_selectors;

/// A column with an index and type
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Column<C: ColumnType> {
    pub index: usize,
    pub column_type: C,
}

impl From<Column<Any>> for metadata::Column {
    fn from(val: Column<Any>) -> Self {
        metadata::Column {
            index: val.index(),
            column_type: *val.column_type(),
        }
    }
}

// TODO: Remove all these methods, and directly access the fields?
impl<C: ColumnType> Column<C> {
    pub fn new(index: usize, column_type: C) -> Self {
        Column { index, column_type }
    }

    /// Index of this column.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Type of this column.
    pub fn column_type(&self) -> &C {
        &self.column_type
    }

    /// Return expression from column at a relative position
    pub fn query_cell<F: Field>(&self, at: Rotation) -> Expression<F> {
        let expr_mid = self.column_type.query_cell::<F>(self.index, at);
        match expr_mid {
            ExpressionMid::Advice(q) => Expression::Advice(AdviceQuery {
                index: None,
                column_index: q.column_index,
                rotation: q.rotation,
                phase: sealed::Phase(q.phase),
            }),
            ExpressionMid::Fixed(q) => Expression::Fixed(FixedQuery {
                index: None,
                column_index: q.column_index,
                rotation: q.rotation,
            }),
            ExpressionMid::Instance(q) => Expression::Instance(InstanceQuery {
                index: None,
                column_index: q.column_index,
                rotation: q.rotation,
            }),
            _ => unreachable!(),
        }
    }

    /// Return expression from column at the current row
    pub fn cur<F: Field>(&self) -> Expression<F> {
        self.query_cell(Rotation::cur())
    }

    /// Return expression from column at the next row
    pub fn next<F: Field>(&self) -> Expression<F> {
        self.query_cell(Rotation::next())
    }

    /// Return expression from column at the previous row
    pub fn prev<F: Field>(&self) -> Expression<F> {
        self.query_cell(Rotation::prev())
    }

    /// Return expression from column at the specified rotation
    pub fn rot<F: Field>(&self, rotation: i32) -> Expression<F> {
        self.query_cell(Rotation(rotation))
    }
}

impl<C: ColumnType> Ord for Column<C> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // This ordering is consensus-critical! The layouters rely on deterministic column
        // orderings.
        match self.column_type.into().cmp(&other.column_type.into()) {
            // Indices are assigned within column types.
            std::cmp::Ordering::Equal => self.index.cmp(&other.index),
            order => order,
        }
    }
}

impl<C: ColumnType> PartialOrd for Column<C> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl From<ColumnMid> for Column<Any> {
    fn from(column: ColumnMid) -> Column<Any> {
        Column {
            index: column.index,
            column_type: column.column_type,
        }
    }
}

impl From<Column<Any>> for ColumnMid {
    fn from(val: Column<Any>) -> Self {
        ColumnMid {
            index: val.index(),
            column_type: *val.column_type(),
        }
    }
}

impl From<Column<Advice>> for Column<Any> {
    fn from(advice: Column<Advice>) -> Column<Any> {
        Column {
            index: advice.index(),
            column_type: Any::Advice(advice.column_type),
        }
    }
}

impl From<Column<Fixed>> for Column<Any> {
    fn from(advice: Column<Fixed>) -> Column<Any> {
        Column {
            index: advice.index(),
            column_type: Any::Fixed,
        }
    }
}

impl From<Column<Instance>> for Column<Any> {
    fn from(advice: Column<Instance>) -> Column<Any> {
        Column {
            index: advice.index(),
            column_type: Any::Instance,
        }
    }
}

impl TryFrom<Column<Any>> for Column<Advice> {
    type Error = &'static str;

    fn try_from(any: Column<Any>) -> Result<Self, Self::Error> {
        match any.column_type() {
            Any::Advice(advice) => Ok(Column {
                index: any.index(),
                column_type: *advice,
            }),
            _ => Err("Cannot convert into Column<Advice>"),
        }
    }
}

impl TryFrom<Column<Any>> for Column<Fixed> {
    type Error = &'static str;

    fn try_from(any: Column<Any>) -> Result<Self, Self::Error> {
        match any.column_type() {
            Any::Fixed => Ok(Column {
                index: any.index(),
                column_type: Fixed,
            }),
            _ => Err("Cannot convert into Column<Fixed>"),
        }
    }
}

impl TryFrom<Column<Any>> for Column<Instance> {
    type Error = &'static str;

    fn try_from(any: Column<Any>) -> Result<Self, Self::Error> {
        match any.column_type() {
            Any::Instance => Ok(Column {
                index: any.index(),
                column_type: Instance,
            }),
            _ => Err("Cannot convert into Column<Instance>"),
        }
    }
}

// TODO: Move sealed phase to frontend, and always use u8 in middleware and backend
pub mod sealed {
    /// Phase of advice column
    #[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    pub struct Phase(pub u8);

    impl Phase {
        pub fn prev(&self) -> Option<Phase> {
            self.0.checked_sub(1).map(Phase)
        }
    }

    impl SealedPhase for Phase {
        fn to_sealed(self) -> Phase {
            self
        }
    }

    /// Sealed trait to help keep `Phase` private.
    pub trait SealedPhase {
        fn to_sealed(self) -> Phase;
    }
}

/// Phase of advice column
pub trait Phase: SealedPhase {}

impl<P: SealedPhase> Phase for P {}

/// First phase
#[derive(Debug)]
pub struct FirstPhase;

impl SealedPhase for super::FirstPhase {
    fn to_sealed(self) -> sealed::Phase {
        sealed::Phase(0)
    }
}

/// Second phase
#[derive(Debug)]
pub struct SecondPhase;

impl SealedPhase for super::SecondPhase {
    fn to_sealed(self) -> sealed::Phase {
        sealed::Phase(1)
    }
}

/// Third phase
#[derive(Debug)]
pub struct ThirdPhase;

impl SealedPhase for super::ThirdPhase {
    fn to_sealed(self) -> sealed::Phase {
        sealed::Phase(2)
    }
}

/// A selector, representing a fixed boolean value per row of the circuit.
///
/// Selectors can be used to conditionally enable (portions of) gates:
/// ```
/// use halo2_middleware::poly::Rotation;
/// # use halo2curves::pasta::Fp;
/// # use halo2_common::plonk::ConstraintSystem;
///
/// # let mut meta = ConstraintSystem::<Fp>::default();
/// let a = meta.advice_column();
/// let b = meta.advice_column();
/// let s = meta.selector();
///
/// meta.create_gate("foo", |meta| {
///     let a = meta.query_advice(a, Rotation::prev());
///     let b = meta.query_advice(b, Rotation::cur());
///     let s = meta.query_selector(s);
///
///     // On rows where the selector is enabled, a is constrained to equal b.
///     // On rows where the selector is disabled, a and b can take any value.
///     vec![s * (a - b)]
/// });
/// ```
///
/// Selectors are disabled on all rows by default, and must be explicitly enabled on each
/// row when required:
/// ```
/// use halo2_middleware::circuit::Advice;
/// use halo2_common::circuit::{Chip, Layouter, Value};
/// use halo2_common::plonk::circuit::{Column, Selector};
/// use halo2_common::plonk::Error;
/// use halo2_middleware::ff::Field;
/// # use halo2_middleware::circuit::Fixed;
///
/// struct Config {
///     a: Column<Advice>,
///     b: Column<Advice>,
///     s: Selector,
/// }
///
/// fn circuit_logic<F: Field, C: Chip<F>>(chip: C, mut layouter: impl Layouter<F>) -> Result<(), Error> {
///     let config = chip.config();
///     # let config: Config = todo!();
///     layouter.assign_region(|| "bar", |mut region| {
///         region.assign_advice(|| "a", config.a, 0, || Value::known(F::ONE))?;
///         region.assign_advice(|| "a", config.b, 1, || Value::known(F::ONE))?;
///         config.s.enable(&mut region, 1)
///     })?;
///     Ok(())
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Selector(pub usize, bool);

impl Selector {
    /// Enable this selector at the given offset within the given region.
    pub fn enable<F: Field>(&self, region: &mut Region<F>, offset: usize) -> Result<(), Error> {
        region.enable_selector(|| "", self, offset)
    }

    /// Is this selector "simple"? Simple selectors can only be multiplied
    /// by expressions that contain no other simple selectors.
    pub fn is_simple(&self) -> bool {
        self.1
    }

    /// Returns index of this selector
    pub fn index(&self) -> usize {
        self.0
    }

    /// Return expression from selector
    pub fn expr<F: Field>(&self) -> Expression<F> {
        Expression::Selector(*self)
    }
}

/// Query of fixed column at a certain relative location
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FixedQuery {
    /// Query index
    pub index: Option<usize>,
    /// Column index
    pub column_index: usize,
    /// Rotation of this query
    pub rotation: Rotation,
}

impl FixedQuery {
    /// Column index
    pub fn column_index(&self) -> usize {
        self.column_index
    }

    /// Rotation of this query
    pub fn rotation(&self) -> Rotation {
        self.rotation
    }
}

/// Query of advice column at a certain relative location
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct AdviceQuery {
    /// Query index
    pub index: Option<usize>,
    /// Column index
    pub column_index: usize,
    /// Rotation of this query
    pub rotation: Rotation,
    /// Phase of this advice column
    pub phase: sealed::Phase,
}

impl AdviceQuery {
    /// Column index
    pub fn column_index(&self) -> usize {
        self.column_index
    }

    /// Rotation of this query
    pub fn rotation(&self) -> Rotation {
        self.rotation
    }

    /// Phase of this advice column
    pub fn phase(&self) -> u8 {
        self.phase.0
    }
}

/// Query of instance column at a certain relative location
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct InstanceQuery {
    /// Query index
    pub index: Option<usize>,
    /// Column index
    pub column_index: usize,
    /// Rotation of this query
    pub rotation: Rotation,
}

impl InstanceQuery {
    /// Column index
    pub fn column_index(&self) -> usize {
        self.column_index
    }

    /// Rotation of this query
    pub fn rotation(&self) -> Rotation {
        self.rotation
    }
}

/// A fixed column of a lookup table.
///
/// A lookup table can be loaded into this column via [`Layouter::assign_table`]. Columns
/// can currently only contain a single table, but they may be used in multiple lookup
/// arguments via [`ConstraintSystem::lookup`].
///
/// Lookup table columns are always "encumbered" by the lookup arguments they are used in;
/// they cannot simultaneously be used as general fixed columns.
///
/// [`Layouter::assign_table`]: crate::circuit::Layouter::assign_table
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct TableColumn {
    /// The fixed column that this table column is stored in.
    ///
    /// # Security
    ///
    /// This inner column MUST NOT be exposed in the public API, or else chip developers
    /// can load lookup tables into their circuits without default-value-filling the
    /// columns, which can cause soundness bugs.
    inner: Column<Fixed>,
}

impl TableColumn {
    /// Returns inner column
    pub fn inner(&self) -> Column<Fixed> {
        self.inner
    }
}

/// A challenge squeezed from transcript after advice columns at the phase have been committed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Challenge {
    pub index: usize,
    pub(crate) phase: u8,
}

impl Challenge {
    /// Index of this challenge.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Phase of this challenge.
    pub fn phase(&self) -> u8 {
        self.phase
    }

    /// Return Expression
    pub fn expr<F: Field>(&self) -> Expression<F> {
        Expression::Challenge(*self)
    }
}

impl From<Challenge> for ChallengeMid {
    fn from(val: Challenge) -> Self {
        ChallengeMid {
            index: val.index,
            phase: val.phase,
        }
    }
}

impl From<ChallengeMid> for Challenge {
    fn from(c: ChallengeMid) -> Self {
        Self {
            index: c.index,
            phase: c.phase,
        }
    }
}

/// This trait allows a [`Circuit`] to direct some backend to assign a witness
/// for a constraint system.
pub trait Assignment<F: Field> {
    /// Creates a new region and enters into it.
    ///
    /// Panics if we are currently in a region (if `exit_region` was not called).
    ///
    /// Not intended for downstream consumption; use [`Layouter::assign_region`] instead.
    ///
    /// [`Layouter::assign_region`]: crate::circuit::Layouter#method.assign_region
    fn enter_region<NR, N>(&mut self, name_fn: N)
    where
        NR: Into<String>,
        N: FnOnce() -> NR;

    /// Allows the developer to include an annotation for an specific column within a `Region`.
    ///
    /// This is usually useful for debugging circuit failures.
    fn annotate_column<A, AR>(&mut self, annotation: A, column: Column<Any>)
    where
        A: FnOnce() -> AR,
        AR: Into<String>;

    /// Exits the current region.
    ///
    /// Panics if we are not currently in a region (if `enter_region` was not called).
    ///
    /// Not intended for downstream consumption; use [`Layouter::assign_region`] instead.
    ///
    /// [`Layouter::assign_region`]: crate::circuit::Layouter#method.assign_region
    fn exit_region(&mut self);

    /// Enables a selector at the given row.
    fn enable_selector<A, AR>(
        &mut self,
        annotation: A,
        selector: &Selector,
        row: usize,
    ) -> Result<(), Error>
    where
        A: FnOnce() -> AR,
        AR: Into<String>;

    /// Queries the cell of an instance column at a particular absolute row.
    ///
    /// Returns the cell's value, if known.
    fn query_instance(&self, column: Column<Instance>, row: usize) -> Result<Value<F>, Error>;

    /// Assign an advice column value (witness)
    fn assign_advice<V, VR, A, AR>(
        &mut self,
        annotation: A,
        column: Column<Advice>,
        row: usize,
        to: V,
    ) -> Result<(), Error>
    where
        V: FnOnce() -> Value<VR>,
        VR: Into<Assigned<F>>,
        A: FnOnce() -> AR,
        AR: Into<String>;

    /// Assign a fixed value
    fn assign_fixed<V, VR, A, AR>(
        &mut self,
        annotation: A,
        column: Column<Fixed>,
        row: usize,
        to: V,
    ) -> Result<(), Error>
    where
        V: FnOnce() -> Value<VR>,
        VR: Into<Assigned<F>>,
        A: FnOnce() -> AR,
        AR: Into<String>;

    /// Assign two cells to have the same value
    fn copy(
        &mut self,
        left_column: Column<Any>,
        left_row: usize,
        right_column: Column<Any>,
        right_row: usize,
    ) -> Result<(), Error>;

    /// Fills a fixed `column` starting from the given `row` with value `to`.
    fn fill_from_row(
        &mut self,
        column: Column<Fixed>,
        row: usize,
        to: Value<Assigned<F>>,
    ) -> Result<(), Error>;

    /// Queries the value of the given challenge.
    ///
    /// Returns `Value::unknown()` if the current synthesis phase is before the challenge can be queried.
    fn get_challenge(&self, challenge: Challenge) -> Value<F>;

    /// Creates a new (sub)namespace and enters into it.
    ///
    /// Not intended for downstream consumption; use [`Layouter::namespace`] instead.
    ///
    /// [`Layouter::namespace`]: crate::circuit::Layouter#method.namespace
    fn push_namespace<NR, N>(&mut self, name_fn: N)
    where
        NR: Into<String>,
        N: FnOnce() -> NR;

    /// Exits out of the existing namespace.
    ///
    /// Not intended for downstream consumption; use [`Layouter::namespace`] instead.
    ///
    /// [`Layouter::namespace`]: crate::circuit::Layouter#method.namespace
    fn pop_namespace(&mut self, gadget_name: Option<String>);
}

/// A floor planning strategy for a circuit.
///
/// The floor planner is chip-agnostic and applies its strategy to the circuit it is used
/// within.
pub trait FloorPlanner {
    /// Given the provided `cs`, synthesize the given circuit.
    ///
    /// `constants` is the list of fixed columns that the layouter may use to assign
    /// global constant values. These columns will all have been equality-enabled.
    ///
    /// Internally, a floor planner will perform the following operations:
    /// - Instantiate a [`Layouter`] for this floor planner.
    /// - Perform any necessary setup or measurement tasks, which may involve one or more
    ///   calls to `Circuit::default().synthesize(config, &mut layouter)`.
    /// - Call `circuit.synthesize(config, &mut layouter)` exactly once.
    fn synthesize<F: Field, CS: Assignment<F> + SyncDeps, C: Circuit<F>>(
        cs: &mut CS,
        circuit: &C,
        config: C::Config,
        constants: Vec<Column<Fixed>>,
    ) -> Result<(), Error>;
}

/// This is a trait that circuits provide implementations for so that the
/// backend prover can ask the circuit to synthesize using some given
/// [`ConstraintSystem`] implementation.
pub trait Circuit<F: Field> {
    /// This is a configuration object that stores things like columns.
    type Config: Clone;
    /// The floor planner used for this circuit. This is an associated type of the
    /// `Circuit` trait because its behaviour is circuit-critical.
    type FloorPlanner: FloorPlanner;
    /// Optional circuit configuration parameters. Requires the `circuit-params` feature.
    #[cfg(feature = "circuit-params")]
    type Params: Default;

    /// Returns a copy of this circuit with no witness values (i.e. all witnesses set to
    /// `None`). For most circuits, this will be equal to `Self::default()`.
    fn without_witnesses(&self) -> Self;

    /// Returns a reference to the parameters that should be used to configure the circuit.
    /// Requires the `circuit-params` feature.
    #[cfg(feature = "circuit-params")]
    fn params(&self) -> Self::Params {
        Self::Params::default()
    }

    /// The circuit is given an opportunity to describe the exact gate
    /// arrangement, column arrangement, etc.  Takes a runtime parameter.  The default
    /// implementation calls `configure` ignoring the `_params` argument in order to easily support
    /// circuits that don't use configuration parameters.
    #[cfg(feature = "circuit-params")]
    fn configure_with_params(
        meta: &mut ConstraintSystem<F>,
        _params: Self::Params,
    ) -> Self::Config {
        Self::configure(meta)
    }

    /// The circuit is given an opportunity to describe the exact gate
    /// arrangement, column arrangement, etc.
    fn configure(meta: &mut ConstraintSystem<F>) -> Self::Config;

    /// Given the provided `cs`, synthesize the circuit. The concrete type of
    /// the caller will be different depending on the context, and they may or
    /// may not expect to have a witness present.
    fn synthesize(&self, config: Self::Config, layouter: impl Layouter<F>) -> Result<(), Error>;
}

// TODO: Create two types from this, one with selector for the frontend (this way we can move the
// Layouter traits, Region and Selector to frontend).  And one without selector for the backend.
/// Low-degree expression representing an identity that must hold over the committed columns.
#[derive(Clone, PartialEq, Eq)]
pub enum Expression<F> {
    /// This is a constant polynomial
    Constant(F),
    /// This is a virtual selector
    Selector(Selector),
    /// This is a fixed column queried at a certain relative location
    Fixed(FixedQuery),
    /// This is an advice (witness) column queried at a certain relative location
    Advice(AdviceQuery),
    /// This is an instance (external) column queried at a certain relative location
    Instance(InstanceQuery),
    /// This is a challenge
    Challenge(Challenge),
    /// This is a negated polynomial
    Negated(Box<Expression<F>>),
    /// This is the sum of two polynomials
    Sum(Box<Expression<F>>, Box<Expression<F>>),
    /// This is the product of two polynomials
    Product(Box<Expression<F>>, Box<Expression<F>>),
    /// This is a scaled polynomial
    Scaled(Box<Expression<F>>, F),
}

impl<F> From<Expression<F>> for ExpressionMid<F> {
    fn from(val: Expression<F>) -> Self {
        match val {
            Expression::Constant(c) => ExpressionMid::Constant(c),
            Expression::Selector(_) => unreachable!(),
            Expression::Fixed(FixedQuery {
                column_index,
                rotation,
                ..
            }) => ExpressionMid::Fixed(FixedQueryMid {
                column_index,
                rotation,
            }),
            Expression::Advice(AdviceQuery {
                column_index,
                rotation,
                phase,
                ..
            }) => ExpressionMid::Advice(AdviceQueryMid {
                column_index,
                rotation,
                phase: phase.0,
            }),
            Expression::Instance(InstanceQuery {
                column_index,
                rotation,
                ..
            }) => ExpressionMid::Instance(InstanceQueryMid {
                column_index,
                rotation,
            }),
            Expression::Challenge(c) => ExpressionMid::Challenge(c.into()),
            Expression::Negated(e) => ExpressionMid::Negated(Box::new((*e).into())),
            Expression::Sum(lhs, rhs) => {
                ExpressionMid::Sum(Box::new((*lhs).into()), Box::new((*rhs).into()))
            }
            Expression::Product(lhs, rhs) => {
                ExpressionMid::Product(Box::new((*lhs).into()), Box::new((*rhs).into()))
            }
            Expression::Scaled(e, c) => ExpressionMid::Scaled(Box::new((*e).into()), c),
        }
    }
}

impl<F: Field> Expression<F> {
    /// Make side effects
    pub fn query_cells(&mut self, cells: &mut VirtualCells<'_, F>) {
        match self {
            Expression::Constant(_) => (),
            Expression::Selector(selector) => {
                if !cells.queried_selectors.contains(selector) {
                    cells.queried_selectors.push(*selector);
                }
            }
            Expression::Fixed(query) => {
                if query.index.is_none() {
                    let col = Column {
                        index: query.column_index,
                        column_type: Fixed,
                    };
                    cells.queried_cells.push((col, query.rotation).into());
                    query.index = Some(cells.meta.query_fixed_index(col, query.rotation));
                }
            }
            Expression::Advice(query) => {
                if query.index.is_none() {
                    let col = Column {
                        index: query.column_index,
                        column_type: Advice {
                            phase: query.phase.0,
                        },
                    };
                    cells.queried_cells.push((col, query.rotation).into());
                    query.index = Some(cells.meta.query_advice_index(col, query.rotation));
                }
            }
            Expression::Instance(query) => {
                if query.index.is_none() {
                    let col = Column {
                        index: query.column_index,
                        column_type: Instance,
                    };
                    cells.queried_cells.push((col, query.rotation).into());
                    query.index = Some(cells.meta.query_instance_index(col, query.rotation));
                }
            }
            Expression::Challenge(_) => (),
            Expression::Negated(a) => a.query_cells(cells),
            Expression::Sum(a, b) => {
                a.query_cells(cells);
                b.query_cells(cells);
            }
            Expression::Product(a, b) => {
                a.query_cells(cells);
                b.query_cells(cells);
            }
            Expression::Scaled(a, _) => a.query_cells(cells),
        };
    }

    /// Evaluate the polynomial using the provided closures to perform the
    /// operations.
    #[allow(clippy::too_many_arguments)]
    pub fn evaluate<T>(
        &self,
        constant: &impl Fn(F) -> T,
        selector_column: &impl Fn(Selector) -> T,
        fixed_column: &impl Fn(FixedQuery) -> T,
        advice_column: &impl Fn(AdviceQuery) -> T,
        instance_column: &impl Fn(InstanceQuery) -> T,
        challenge: &impl Fn(Challenge) -> T,
        negated: &impl Fn(T) -> T,
        sum: &impl Fn(T, T) -> T,
        product: &impl Fn(T, T) -> T,
        scaled: &impl Fn(T, F) -> T,
    ) -> T {
        match self {
            Expression::Constant(scalar) => constant(*scalar),
            Expression::Selector(selector) => selector_column(*selector),
            Expression::Fixed(query) => fixed_column(*query),
            Expression::Advice(query) => advice_column(*query),
            Expression::Instance(query) => instance_column(*query),
            Expression::Challenge(value) => challenge(*value),
            Expression::Negated(a) => {
                let a = a.evaluate(
                    constant,
                    selector_column,
                    fixed_column,
                    advice_column,
                    instance_column,
                    challenge,
                    negated,
                    sum,
                    product,
                    scaled,
                );
                negated(a)
            }
            Expression::Sum(a, b) => {
                let a = a.evaluate(
                    constant,
                    selector_column,
                    fixed_column,
                    advice_column,
                    instance_column,
                    challenge,
                    negated,
                    sum,
                    product,
                    scaled,
                );
                let b = b.evaluate(
                    constant,
                    selector_column,
                    fixed_column,
                    advice_column,
                    instance_column,
                    challenge,
                    negated,
                    sum,
                    product,
                    scaled,
                );
                sum(a, b)
            }
            Expression::Product(a, b) => {
                let a = a.evaluate(
                    constant,
                    selector_column,
                    fixed_column,
                    advice_column,
                    instance_column,
                    challenge,
                    negated,
                    sum,
                    product,
                    scaled,
                );
                let b = b.evaluate(
                    constant,
                    selector_column,
                    fixed_column,
                    advice_column,
                    instance_column,
                    challenge,
                    negated,
                    sum,
                    product,
                    scaled,
                );
                product(a, b)
            }
            Expression::Scaled(a, f) => {
                let a = a.evaluate(
                    constant,
                    selector_column,
                    fixed_column,
                    advice_column,
                    instance_column,
                    challenge,
                    negated,
                    sum,
                    product,
                    scaled,
                );
                scaled(a, *f)
            }
        }
    }

    /// Evaluate the polynomial lazily using the provided closures to perform the
    /// operations.
    #[allow(clippy::too_many_arguments)]
    pub fn evaluate_lazy<T: PartialEq>(
        &self,
        constant: &impl Fn(F) -> T,
        selector_column: &impl Fn(Selector) -> T,
        fixed_column: &impl Fn(FixedQuery) -> T,
        advice_column: &impl Fn(AdviceQuery) -> T,
        instance_column: &impl Fn(InstanceQuery) -> T,
        challenge: &impl Fn(Challenge) -> T,
        negated: &impl Fn(T) -> T,
        sum: &impl Fn(T, T) -> T,
        product: &impl Fn(T, T) -> T,
        scaled: &impl Fn(T, F) -> T,
        zero: &T,
    ) -> T {
        match self {
            Expression::Constant(scalar) => constant(*scalar),
            Expression::Selector(selector) => selector_column(*selector),
            Expression::Fixed(query) => fixed_column(*query),
            Expression::Advice(query) => advice_column(*query),
            Expression::Instance(query) => instance_column(*query),
            Expression::Challenge(value) => challenge(*value),
            Expression::Negated(a) => {
                let a = a.evaluate_lazy(
                    constant,
                    selector_column,
                    fixed_column,
                    advice_column,
                    instance_column,
                    challenge,
                    negated,
                    sum,
                    product,
                    scaled,
                    zero,
                );
                negated(a)
            }
            Expression::Sum(a, b) => {
                let a = a.evaluate_lazy(
                    constant,
                    selector_column,
                    fixed_column,
                    advice_column,
                    instance_column,
                    challenge,
                    negated,
                    sum,
                    product,
                    scaled,
                    zero,
                );
                let b = b.evaluate_lazy(
                    constant,
                    selector_column,
                    fixed_column,
                    advice_column,
                    instance_column,
                    challenge,
                    negated,
                    sum,
                    product,
                    scaled,
                    zero,
                );
                sum(a, b)
            }
            Expression::Product(a, b) => {
                let (a, b) = if a.complexity() <= b.complexity() {
                    (a, b)
                } else {
                    (b, a)
                };
                let a = a.evaluate_lazy(
                    constant,
                    selector_column,
                    fixed_column,
                    advice_column,
                    instance_column,
                    challenge,
                    negated,
                    sum,
                    product,
                    scaled,
                    zero,
                );

                if a == *zero {
                    a
                } else {
                    let b = b.evaluate_lazy(
                        constant,
                        selector_column,
                        fixed_column,
                        advice_column,
                        instance_column,
                        challenge,
                        negated,
                        sum,
                        product,
                        scaled,
                        zero,
                    );
                    product(a, b)
                }
            }
            Expression::Scaled(a, f) => {
                let a = a.evaluate_lazy(
                    constant,
                    selector_column,
                    fixed_column,
                    advice_column,
                    instance_column,
                    challenge,
                    negated,
                    sum,
                    product,
                    scaled,
                    zero,
                );
                scaled(a, *f)
            }
        }
    }

    fn write_identifier<W: std::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        match self {
            Expression::Constant(scalar) => write!(writer, "{scalar:?}"),
            Expression::Selector(selector) => write!(writer, "selector[{}]", selector.0),
            Expression::Fixed(query) => {
                write!(
                    writer,
                    "fixed[{}][{}]",
                    query.column_index, query.rotation.0
                )
            }
            Expression::Advice(query) => {
                write!(
                    writer,
                    "advice[{}][{}]",
                    query.column_index, query.rotation.0
                )
            }
            Expression::Instance(query) => {
                write!(
                    writer,
                    "instance[{}][{}]",
                    query.column_index, query.rotation.0
                )
            }
            Expression::Challenge(challenge) => {
                write!(writer, "challenge[{}]", challenge.index())
            }
            Expression::Negated(a) => {
                writer.write_all(b"(-")?;
                a.write_identifier(writer)?;
                writer.write_all(b")")
            }
            Expression::Sum(a, b) => {
                writer.write_all(b"(")?;
                a.write_identifier(writer)?;
                writer.write_all(b"+")?;
                b.write_identifier(writer)?;
                writer.write_all(b")")
            }
            Expression::Product(a, b) => {
                writer.write_all(b"(")?;
                a.write_identifier(writer)?;
                writer.write_all(b"*")?;
                b.write_identifier(writer)?;
                writer.write_all(b")")
            }
            Expression::Scaled(a, f) => {
                a.write_identifier(writer)?;
                write!(writer, "*{f:?}")
            }
        }
    }

    /// Identifier for this expression. Expressions with identical identifiers
    /// do the same calculation (but the expressions don't need to be exactly equal
    /// in how they are composed e.g. `1 + 2` and `2 + 1` can have the same identifier).
    pub fn identifier(&self) -> String {
        let mut cursor = std::io::Cursor::new(Vec::new());
        self.write_identifier(&mut cursor).unwrap();
        String::from_utf8(cursor.into_inner()).unwrap()
    }

    /// Compute the degree of this polynomial
    pub fn degree(&self) -> usize {
        match self {
            Expression::Constant(_) => 0,
            Expression::Selector(_) => 1,
            Expression::Fixed(_) => 1,
            Expression::Advice(_) => 1,
            Expression::Instance(_) => 1,
            Expression::Challenge(_) => 0,
            Expression::Negated(poly) => poly.degree(),
            Expression::Sum(a, b) => max(a.degree(), b.degree()),
            Expression::Product(a, b) => a.degree() + b.degree(),
            Expression::Scaled(poly, _) => poly.degree(),
        }
    }

    /// Approximate the computational complexity of this expression.
    pub fn complexity(&self) -> usize {
        match self {
            Expression::Constant(_) => 0,
            Expression::Selector(_) => 1,
            Expression::Fixed(_) => 1,
            Expression::Advice(_) => 1,
            Expression::Instance(_) => 1,
            Expression::Challenge(_) => 0,
            Expression::Negated(poly) => poly.complexity() + 5,
            Expression::Sum(a, b) => a.complexity() + b.complexity() + 15,
            Expression::Product(a, b) => a.complexity() + b.complexity() + 30,
            Expression::Scaled(poly, _) => poly.complexity() + 30,
        }
    }

    /// Square this expression.
    pub fn square(self) -> Self {
        self.clone() * self
    }

    /// Returns whether or not this expression contains a simple `Selector`.
    fn contains_simple_selector(&self) -> bool {
        self.evaluate(
            &|_| false,
            &|selector| selector.is_simple(),
            &|_| false,
            &|_| false,
            &|_| false,
            &|_| false,
            &|a| a,
            &|a, b| a || b,
            &|a, b| a || b,
            &|a, _| a,
        )
    }

    /// Extracts a simple selector from this gate, if present
    fn extract_simple_selector(&self) -> Option<Selector> {
        let op = |a, b| match (a, b) {
            (Some(a), None) | (None, Some(a)) => Some(a),
            (Some(_), Some(_)) => panic!("two simple selectors cannot be in the same expression"),
            _ => None,
        };

        self.evaluate(
            &|_| None,
            &|selector| {
                if selector.is_simple() {
                    Some(selector)
                } else {
                    None
                }
            },
            &|_| None,
            &|_| None,
            &|_| None,
            &|_| None,
            &|a| a,
            &op,
            &op,
            &|a, _| a,
        )
    }
}

impl<F: std::fmt::Debug> std::fmt::Debug for Expression<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Expression::Constant(scalar) => f.debug_tuple("Constant").field(scalar).finish(),
            Expression::Selector(selector) => f.debug_tuple("Selector").field(selector).finish(),
            // Skip enum variant and print query struct directly to maintain backwards compatibility.
            Expression::Fixed(query) => {
                let mut debug_struct = f.debug_struct("Fixed");
                match query.index {
                    None => debug_struct.field("query_index", &query.index),
                    Some(idx) => debug_struct.field("query_index", &idx),
                };
                debug_struct
                    .field("column_index", &query.column_index)
                    .field("rotation", &query.rotation)
                    .finish()
            }
            Expression::Advice(query) => {
                let mut debug_struct = f.debug_struct("Advice");
                match query.index {
                    None => debug_struct.field("query_index", &query.index),
                    Some(idx) => debug_struct.field("query_index", &idx),
                };
                debug_struct
                    .field("column_index", &query.column_index)
                    .field("rotation", &query.rotation);
                // Only show advice's phase if it's not in first phase.
                if query.phase != FirstPhase.to_sealed() {
                    debug_struct.field("phase", &query.phase);
                }
                debug_struct.finish()
            }
            Expression::Instance(query) => {
                let mut debug_struct = f.debug_struct("Instance");
                match query.index {
                    None => debug_struct.field("query_index", &query.index),
                    Some(idx) => debug_struct.field("query_index", &idx),
                };
                debug_struct
                    .field("column_index", &query.column_index)
                    .field("rotation", &query.rotation)
                    .finish()
            }
            Expression::Challenge(challenge) => {
                f.debug_tuple("Challenge").field(challenge).finish()
            }
            Expression::Negated(poly) => f.debug_tuple("Negated").field(poly).finish(),
            Expression::Sum(a, b) => f.debug_tuple("Sum").field(a).field(b).finish(),
            Expression::Product(a, b) => f.debug_tuple("Product").field(a).field(b).finish(),
            Expression::Scaled(poly, scalar) => {
                f.debug_tuple("Scaled").field(poly).field(scalar).finish()
            }
        }
    }
}

impl<F: Field> Neg for Expression<F> {
    type Output = Expression<F>;
    fn neg(self) -> Self::Output {
        Expression::Negated(Box::new(self))
    }
}

impl<F: Field> Add for Expression<F> {
    type Output = Expression<F>;
    fn add(self, rhs: Expression<F>) -> Expression<F> {
        if self.contains_simple_selector() || rhs.contains_simple_selector() {
            panic!("attempted to use a simple selector in an addition");
        }
        Expression::Sum(Box::new(self), Box::new(rhs))
    }
}

impl<F: Field> Sub for Expression<F> {
    type Output = Expression<F>;
    fn sub(self, rhs: Expression<F>) -> Expression<F> {
        if self.contains_simple_selector() || rhs.contains_simple_selector() {
            panic!("attempted to use a simple selector in a subtraction");
        }
        Expression::Sum(Box::new(self), Box::new(-rhs))
    }
}

impl<F: Field> Mul for Expression<F> {
    type Output = Expression<F>;
    fn mul(self, rhs: Expression<F>) -> Expression<F> {
        if self.contains_simple_selector() && rhs.contains_simple_selector() {
            panic!("attempted to multiply two expressions containing simple selectors");
        }
        Expression::Product(Box::new(self), Box::new(rhs))
    }
}

impl<F: Field> Mul<F> for Expression<F> {
    type Output = Expression<F>;
    fn mul(self, rhs: F) -> Expression<F> {
        Expression::Scaled(Box::new(self), rhs)
    }
}

impl<F: Field> Sum<Self> for Expression<F> {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.reduce(|acc, x| acc + x)
            .unwrap_or(Expression::Constant(F::ZERO))
    }
}

impl<F: Field> Product<Self> for Expression<F> {
    fn product<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.reduce(|acc, x| acc * x)
            .unwrap_or(Expression::Constant(F::ONE))
    }
}

/// Represents an index into a vector where each entry corresponds to a distinct
/// point that polynomials are queried at.
#[derive(Copy, Clone, Debug)]
pub(crate) struct PointIndex(pub usize);

/// A "virtual cell" is a PLONK cell that has been queried at a particular relative offset
/// within a custom gate.
#[derive(Clone, Debug)]
pub struct VirtualCell {
    pub column: Column<Any>,
    pub rotation: Rotation,
}

impl<Col: Into<Column<Any>>> From<(Col, Rotation)> for VirtualCell {
    fn from((column, rotation): (Col, Rotation)) -> Self {
        VirtualCell {
            column: column.into(),
            rotation,
        }
    }
}

/// An individual polynomial constraint.
///
/// These are returned by the closures passed to `ConstraintSystem::create_gate`.
#[derive(Debug)]
pub struct Constraint<F: Field> {
    name: String,
    poly: Expression<F>,
}

impl<F: Field> From<Expression<F>> for Constraint<F> {
    fn from(poly: Expression<F>) -> Self {
        Constraint {
            name: "".to_string(),
            poly,
        }
    }
}

impl<F: Field, S: AsRef<str>> From<(S, Expression<F>)> for Constraint<F> {
    fn from((name, poly): (S, Expression<F>)) -> Self {
        Constraint {
            name: name.as_ref().to_string(),
            poly,
        }
    }
}

impl<F: Field> From<Expression<F>> for Vec<Constraint<F>> {
    fn from(poly: Expression<F>) -> Self {
        vec![Constraint {
            name: "".to_string(),
            poly,
        }]
    }
}

/// A set of polynomial constraints with a common selector.
///
/// ```
/// use halo2_common::{plonk::{Constraints, Expression}};
/// use halo2_middleware::poly::Rotation;
/// use halo2curves::pasta::Fp;
/// # use halo2_common::plonk::ConstraintSystem;
///
/// # let mut meta = ConstraintSystem::<Fp>::default();
/// let a = meta.advice_column();
/// let b = meta.advice_column();
/// let c = meta.advice_column();
/// let s = meta.selector();
///
/// meta.create_gate("foo", |meta| {
///     let next = meta.query_advice(a, Rotation::next());
///     let a = meta.query_advice(a, Rotation::cur());
///     let b = meta.query_advice(b, Rotation::cur());
///     let c = meta.query_advice(c, Rotation::cur());
///     let s_ternary = meta.query_selector(s);
///
///     let one_minus_a = Expression::Constant(Fp::one()) - a.clone();
///
///     Constraints::with_selector(
///         s_ternary,
///         std::array::IntoIter::new([
///             ("a is boolean", a.clone() * one_minus_a.clone()),
///             ("next == a ? b : c", next - (a * b + one_minus_a * c)),
///         ]),
///     )
/// });
/// ```
///
/// Note that the use of `std::array::IntoIter::new` is only necessary if you need to
/// support Rust 1.51 or 1.52. If your minimum supported Rust version is 1.53 or greater,
/// you can pass an array directly.
#[derive(Debug)]
pub struct Constraints<F: Field, C: Into<Constraint<F>>, Iter: IntoIterator<Item = C>> {
    selector: Expression<F>,
    constraints: Iter,
}

impl<F: Field, C: Into<Constraint<F>>, Iter: IntoIterator<Item = C>> Constraints<F, C, Iter> {
    /// Constructs a set of constraints that are controlled by the given selector.
    ///
    /// Each constraint `c` in `iterator` will be converted into the constraint
    /// `selector * c`.
    pub fn with_selector(selector: Expression<F>, constraints: Iter) -> Self {
        Constraints {
            selector,
            constraints,
        }
    }
}

fn apply_selector_to_constraint<F: Field, C: Into<Constraint<F>>>(
    (selector, c): (Expression<F>, C),
) -> Constraint<F> {
    let constraint: Constraint<F> = c.into();
    Constraint {
        name: constraint.name,
        poly: selector * constraint.poly,
    }
}

type ApplySelectorToConstraint<F, C> = fn((Expression<F>, C)) -> Constraint<F>;
type ConstraintsIterator<F, C, I> = std::iter::Map<
    std::iter::Zip<std::iter::Repeat<Expression<F>>, I>,
    ApplySelectorToConstraint<F, C>,
>;

impl<F: Field, C: Into<Constraint<F>>, Iter: IntoIterator<Item = C>> IntoIterator
    for Constraints<F, C, Iter>
{
    type Item = Constraint<F>;
    type IntoIter = ConstraintsIterator<F, C, Iter::IntoIter>;

    fn into_iter(self) -> Self::IntoIter {
        std::iter::repeat(self.selector)
            .zip(self.constraints)
            .map(apply_selector_to_constraint)
    }
}

/// Gate
#[derive(Clone, Debug)]
pub struct Gate<F: Field> {
    name: String,
    constraint_names: Vec<String>,
    polys: Vec<Expression<F>>,
    /// We track queried selectors separately from other cells, so that we can use them to
    /// trigger debug checks on gates.
    queried_selectors: Vec<Selector>,
    queried_cells: Vec<VirtualCell>,
}

impl<F: Field> Gate<F> {
    /// Returns the gate name.
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    /// Returns the name of the constraint at index `constraint_index`.
    pub fn constraint_name(&self, constraint_index: usize) -> &str {
        self.constraint_names[constraint_index].as_str()
    }

    /// Returns constraints of this gate
    pub fn polynomials(&self) -> &[Expression<F>] {
        &self.polys
    }

    pub fn queried_selectors(&self) -> &[Selector] {
        &self.queried_selectors
    }

    pub fn queried_cells(&self) -> &[VirtualCell] {
        &self.queried_cells
    }
}

struct QueriesMap {
    advice_map: HashMap<(Column<Advice>, Rotation), usize>,
    instance_map: HashMap<(Column<Instance>, Rotation), usize>,
    fixed_map: HashMap<(Column<Fixed>, Rotation), usize>,
    advice: Vec<(Column<Advice>, Rotation)>,
    instance: Vec<(Column<Instance>, Rotation)>,
    fixed: Vec<(Column<Fixed>, Rotation)>,
}

impl QueriesMap {
    fn add_advice(&mut self, col: Column<Advice>, rot: Rotation) -> usize {
        *self.advice_map.entry((col, rot)).or_insert_with(|| {
            self.advice.push((col, rot));
            self.advice.len() - 1
        })
    }
    fn add_instance(&mut self, col: Column<Instance>, rot: Rotation) -> usize {
        *self.instance_map.entry((col, rot)).or_insert_with(|| {
            self.instance.push((col, rot));
            self.instance.len() - 1
        })
    }
    fn add_fixed(&mut self, col: Column<Fixed>, rot: Rotation) -> usize {
        *self.fixed_map.entry((col, rot)).or_insert_with(|| {
            self.fixed.push((col, rot));
            self.fixed.len() - 1
        })
    }
}

impl QueriesMap {
    fn as_expression<F: Field>(&mut self, expr: &ExpressionMid<F>) -> Expression<F> {
        match expr {
            ExpressionMid::Constant(c) => Expression::Constant(*c),
            ExpressionMid::Fixed(query) => {
                let (col, rot) = (Column::new(query.column_index, Fixed), query.rotation);
                let index = self.add_fixed(col, rot);
                Expression::Fixed(FixedQuery {
                    index: Some(index),
                    column_index: query.column_index,
                    rotation: query.rotation,
                })
            }
            ExpressionMid::Advice(query) => {
                let (col, rot) = (
                    Column::new(query.column_index, Advice { phase: query.phase }),
                    query.rotation,
                );
                let index = self.add_advice(col, rot);
                Expression::Advice(AdviceQuery {
                    index: Some(index),
                    column_index: query.column_index,
                    rotation: query.rotation,
                    phase: sealed::Phase(query.phase),
                })
            }
            ExpressionMid::Instance(query) => {
                let (col, rot) = (Column::new(query.column_index, Instance), query.rotation);
                let index = self.add_instance(col, rot);
                Expression::Instance(InstanceQuery {
                    index: Some(index),
                    column_index: query.column_index,
                    rotation: query.rotation,
                })
            }
            ExpressionMid::Challenge(c) => Expression::Challenge((*c).into()),
            ExpressionMid::Negated(e) => Expression::Negated(Box::new(self.as_expression(e))),
            ExpressionMid::Sum(lhs, rhs) => Expression::Sum(
                Box::new(self.as_expression(lhs)),
                Box::new(self.as_expression(rhs)),
            ),
            ExpressionMid::Product(lhs, rhs) => Expression::Product(
                Box::new(self.as_expression(lhs)),
                Box::new(self.as_expression(rhs)),
            ),
            ExpressionMid::Scaled(e, c) => Expression::Scaled(Box::new(self.as_expression(e)), *c),
        }
    }
}

impl<F: Field> From<ConstraintSystem<F>> for ConstraintSystemV2Backend<F> {
    fn from(cs: ConstraintSystem<F>) -> Self {
        ConstraintSystemV2Backend {
            num_fixed_columns: cs.num_fixed_columns,
            num_advice_columns: cs.num_advice_columns,
            num_instance_columns: cs.num_instance_columns,
            num_challenges: cs.num_challenges,
            unblinded_advice_columns: cs.unblinded_advice_columns,
            advice_column_phase: cs.advice_column_phase.iter().map(|p| p.0).collect(),
            challenge_phase: cs.challenge_phase.iter().map(|p| p.0).collect(),
            gates: cs
                .gates
                .into_iter()
                .flat_map(|mut g| {
                    let constraint_names = std::mem::take(&mut g.constraint_names);
                    let gate_name = g.name.clone();
                    g.polys.into_iter().enumerate().map(move |(i, e)| {
                        let name = match constraint_names[i].as_str() {
                            "" => gate_name.clone(),
                            constraint_name => format!("{gate_name}:{constraint_name}"),
                        };
                        GateV2Backend {
                            name,
                            poly: e.into(),
                        }
                    })
                })
                .collect(),
            permutation: halo2_middleware::permutation::ArgumentV2 {
                columns: cs
                    .permutation
                    .columns
                    .into_iter()
                    .map(|c| c.into())
                    .collect(),
            },
            lookups: cs
                .lookups
                .into_iter()
                .map(|l| halo2_middleware::lookup::ArgumentV2 {
                    name: l.name,
                    input_expressions: l.input_expressions.into_iter().map(|e| e.into()).collect(),
                    table_expressions: l.table_expressions.into_iter().map(|e| e.into()).collect(),
                })
                .collect(),
            shuffles: cs
                .shuffles
                .into_iter()
                .map(|s| halo2_middleware::shuffle::ArgumentV2 {
                    name: s.name,
                    input_expressions: s.input_expressions.into_iter().map(|e| e.into()).collect(),
                    shuffle_expressions: s
                        .shuffle_expressions
                        .into_iter()
                        .map(|e| e.into())
                        .collect(),
                })
                .collect(),
            general_column_annotations: cs.general_column_annotations,
        }
    }
}

/// Collect queries used in gates while mapping those gates to equivalent ones with indexed
/// query references in the expressions.
fn cs2_collect_queries_gates<F: Field>(
    cs2: &ConstraintSystemV2Backend<F>,
    queries: &mut QueriesMap,
) -> Vec<Gate<F>> {
    cs2.gates
        .iter()
        .map(|gate| Gate {
            name: gate.name.clone(),
            constraint_names: Vec::new(),
            polys: vec![queries.as_expression(gate.polynomial())],
            queried_selectors: Vec::new(), // Unused?
            queried_cells: Vec::new(),     // Unused?
        })
        .collect()
}

/// Collect queries used in lookups while mapping those lookups to equivalent ones with indexed
/// query references in the expressions.
fn cs2_collect_queries_lookups<F: Field>(
    cs2: &ConstraintSystemV2Backend<F>,
    queries: &mut QueriesMap,
) -> Vec<lookup::Argument<F>> {
    cs2.lookups
        .iter()
        .map(|lookup| lookup::Argument {
            name: lookup.name.clone(),
            input_expressions: lookup
                .input_expressions
                .iter()
                .map(|e| queries.as_expression(e))
                .collect(),
            table_expressions: lookup
                .table_expressions
                .iter()
                .map(|e| queries.as_expression(e))
                .collect(),
        })
        .collect()
}

/// Collect queries used in shuffles while mapping those lookups to equivalent ones with indexed
/// query references in the expressions.
fn cs2_collect_queries_shuffles<F: Field>(
    cs2: &ConstraintSystemV2Backend<F>,
    queries: &mut QueriesMap,
) -> Vec<shuffle::Argument<F>> {
    cs2.shuffles
        .iter()
        .map(|shuffle| shuffle::Argument {
            name: shuffle.name.clone(),
            input_expressions: shuffle
                .input_expressions
                .iter()
                .map(|e| queries.as_expression(e))
                .collect(),
            shuffle_expressions: shuffle
                .shuffle_expressions
                .iter()
                .map(|e| queries.as_expression(e))
                .collect(),
        })
        .collect()
}

/// Collect all queries used in the expressions of gates, lookups and shuffles.  Map the
/// expressions of gates, lookups and shuffles into equivalent ones with indexed query
/// references.
#[allow(clippy::type_complexity)]
pub fn collect_queries<F: Field>(
    cs2: &ConstraintSystemV2Backend<F>,
) -> (
    Queries,
    Vec<Gate<F>>,
    Vec<lookup::Argument<F>>,
    Vec<shuffle::Argument<F>>,
) {
    let mut queries = QueriesMap {
        advice_map: HashMap::new(),
        instance_map: HashMap::new(),
        fixed_map: HashMap::new(),
        advice: Vec::new(),
        instance: Vec::new(),
        fixed: Vec::new(),
    };

    let gates = cs2_collect_queries_gates(cs2, &mut queries);
    let lookups = cs2_collect_queries_lookups(cs2, &mut queries);
    let shuffles = cs2_collect_queries_shuffles(cs2, &mut queries);

    // Each column used in a copy constraint involves a query at rotation current.
    for column in &cs2.permutation.columns {
        match column.column_type {
            Any::Instance => {
                queries.add_instance(Column::new(column.index, Instance), Rotation::cur())
            }
            Any::Fixed => queries.add_fixed(Column::new(column.index, Fixed), Rotation::cur()),
            Any::Advice(advice) => {
                queries.add_advice(Column::new(column.index, advice), Rotation::cur())
            }
        };
    }

    let mut num_advice_queries = vec![0; cs2.num_advice_columns];
    for (column, _) in queries.advice.iter() {
        num_advice_queries[column.index()] += 1;
    }

    let queries = Queries {
        advice: queries.advice,
        instance: queries.instance,
        fixed: queries.fixed,
        num_advice_queries,
    };
    (queries, gates, lookups, shuffles)
}

/// This is a description of the circuit environment, such as the gate, column and
/// permutation arrangements.
#[derive(Debug, Clone)]
pub struct ConstraintSystem<F: Field> {
    pub num_fixed_columns: usize,
    pub num_advice_columns: usize,
    pub num_instance_columns: usize,
    pub num_selectors: usize,
    pub num_challenges: usize,

    /// Contains the index of each advice column that is left unblinded.
    pub unblinded_advice_columns: Vec<usize>,

    /// Contains the phase for each advice column. Should have same length as num_advice_columns.
    pub advice_column_phase: Vec<sealed::Phase>,
    /// Contains the phase for each challenge. Should have same length as num_challenges.
    pub challenge_phase: Vec<sealed::Phase>,

    /// This is a cached vector that maps virtual selectors to the concrete
    /// fixed column that they were compressed into. This is just used by dev
    /// tooling right now.
    pub selector_map: Vec<Column<Fixed>>,

    pub gates: Vec<Gate<F>>,
    pub advice_queries: Vec<(Column<Advice>, Rotation)>,
    // Contains an integer for each advice column
    // identifying how many distinct queries it has
    // so far; should be same length as num_advice_columns.
    pub num_advice_queries: Vec<usize>,
    pub instance_queries: Vec<(Column<Instance>, Rotation)>,
    pub fixed_queries: Vec<(Column<Fixed>, Rotation)>,

    // Permutation argument for performing equality constraints
    pub permutation: permutation::Argument,

    // Vector of lookup arguments, where each corresponds to a sequence of
    // input expressions and a sequence of table expressions involved in the lookup.
    pub lookups: Vec<lookup::Argument<F>>,

    // Vector of shuffle arguments, where each corresponds to a sequence of
    // input expressions and a sequence of shuffle expressions involved in the shuffle.
    pub shuffles: Vec<shuffle::Argument<F>>,

    // List of indexes of Fixed columns which are associated to a circuit-general Column tied to their annotation.
    pub general_column_annotations: HashMap<metadata::Column, String>,

    // Vector of fixed columns, which can be used to store constant values
    // that are copied into advice columns.
    pub constants: Vec<Column<Fixed>>,

    pub minimum_degree: Option<usize>,
}

impl<F: Field> From<ConstraintSystemV2Backend<F>> for ConstraintSystem<F> {
    fn from(cs2: ConstraintSystemV2Backend<F>) -> Self {
        let (queries, gates, lookups, shuffles) = collect_queries(&cs2);
        ConstraintSystem {
            num_fixed_columns: cs2.num_fixed_columns,
            num_advice_columns: cs2.num_advice_columns,
            num_instance_columns: cs2.num_instance_columns,
            num_selectors: 0,
            num_challenges: cs2.num_challenges,
            unblinded_advice_columns: cs2.unblinded_advice_columns,
            advice_column_phase: cs2
                .advice_column_phase
                .into_iter()
                .map(sealed::Phase)
                .collect(),
            challenge_phase: cs2.challenge_phase.into_iter().map(sealed::Phase).collect(),
            selector_map: Vec::new(),
            gates,
            advice_queries: queries.advice,
            num_advice_queries: queries.num_advice_queries,
            instance_queries: queries.instance,
            fixed_queries: queries.fixed,
            permutation: cs2.permutation.into(),
            lookups,
            shuffles,
            general_column_annotations: cs2.general_column_annotations,
            constants: Vec::new(),
            minimum_degree: None,
        }
    }
}

/// Represents the minimal parameters that determine a `ConstraintSystem`.
#[allow(dead_code)]
pub struct PinnedConstraintSystem<'a, F: Field> {
    num_fixed_columns: &'a usize,
    num_advice_columns: &'a usize,
    num_instance_columns: &'a usize,
    num_selectors: &'a usize,
    num_challenges: &'a usize,
    advice_column_phase: &'a Vec<sealed::Phase>,
    challenge_phase: &'a Vec<sealed::Phase>,
    gates: PinnedGates<'a, F>,
    advice_queries: &'a Vec<(Column<Advice>, Rotation)>,
    instance_queries: &'a Vec<(Column<Instance>, Rotation)>,
    fixed_queries: &'a Vec<(Column<Fixed>, Rotation)>,
    permutation: &'a permutation::Argument,
    lookups: &'a Vec<lookup::Argument<F>>,
    shuffles: &'a Vec<shuffle::Argument<F>>,
    constants: &'a Vec<Column<Fixed>>,
    minimum_degree: &'a Option<usize>,
}

impl<'a, F: Field> std::fmt::Debug for PinnedConstraintSystem<'a, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug_struct = f.debug_struct("PinnedConstraintSystem");
        debug_struct
            .field("num_fixed_columns", self.num_fixed_columns)
            .field("num_advice_columns", self.num_advice_columns)
            .field("num_instance_columns", self.num_instance_columns)
            .field("num_selectors", self.num_selectors);
        // Only show multi-phase related fields if it's used.
        if *self.num_challenges > 0 {
            debug_struct
                .field("num_challenges", self.num_challenges)
                .field("advice_column_phase", self.advice_column_phase)
                .field("challenge_phase", self.challenge_phase);
        }
        debug_struct
            .field("gates", &self.gates)
            .field("advice_queries", self.advice_queries)
            .field("instance_queries", self.instance_queries)
            .field("fixed_queries", self.fixed_queries)
            .field("permutation", self.permutation)
            .field("lookups", self.lookups);
        if !self.shuffles.is_empty() {
            debug_struct.field("shuffles", self.shuffles);
        }
        debug_struct
            .field("constants", self.constants)
            .field("minimum_degree", self.minimum_degree);
        debug_struct.finish()
    }
}

struct PinnedGates<'a, F: Field>(&'a Vec<Gate<F>>);

impl<'a, F: Field> std::fmt::Debug for PinnedGates<'a, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_list()
            .entries(self.0.iter().flat_map(|gate| gate.polynomials().iter()))
            .finish()
    }
}

impl<F: Field> Default for ConstraintSystem<F> {
    fn default() -> ConstraintSystem<F> {
        ConstraintSystem {
            num_fixed_columns: 0,
            num_advice_columns: 0,
            num_instance_columns: 0,
            num_selectors: 0,
            num_challenges: 0,
            unblinded_advice_columns: Vec::new(),
            advice_column_phase: Vec::new(),
            challenge_phase: Vec::new(),
            selector_map: vec![],
            gates: vec![],
            fixed_queries: Vec::new(),
            advice_queries: Vec::new(),
            num_advice_queries: Vec::new(),
            instance_queries: Vec::new(),
            permutation: permutation::Argument::default(),
            lookups: Vec::new(),
            shuffles: Vec::new(),
            general_column_annotations: HashMap::new(),
            constants: vec![],
            minimum_degree: None,
        }
    }
}

impl<F: Field> ConstraintSystem<F> {
    /// Obtain a pinned version of this constraint system; a structure with the
    /// minimal parameters needed to determine the rest of the constraint
    /// system.
    pub fn pinned(&self) -> PinnedConstraintSystem<'_, F> {
        PinnedConstraintSystem {
            num_fixed_columns: &self.num_fixed_columns,
            num_advice_columns: &self.num_advice_columns,
            num_instance_columns: &self.num_instance_columns,
            num_selectors: &self.num_selectors,
            num_challenges: &self.num_challenges,
            advice_column_phase: &self.advice_column_phase,
            challenge_phase: &self.challenge_phase,
            gates: PinnedGates(&self.gates),
            fixed_queries: &self.fixed_queries,
            advice_queries: &self.advice_queries,
            instance_queries: &self.instance_queries,
            permutation: &self.permutation,
            lookups: &self.lookups,
            shuffles: &self.shuffles,
            constants: &self.constants,
            minimum_degree: &self.minimum_degree,
        }
    }

    /// Enables this fixed column to be used for global constant assignments.
    ///
    /// # Side-effects
    ///
    /// The column will be equality-enabled.
    pub fn enable_constant(&mut self, column: Column<Fixed>) {
        if !self.constants.contains(&column) {
            self.constants.push(column);
            self.enable_equality(column);
        }
    }

    /// Enable the ability to enforce equality over cells in this column
    pub fn enable_equality<C: Into<Column<Any>>>(&mut self, column: C) {
        let column = column.into();
        self.query_any_index(column, Rotation::cur());
        self.permutation.add_column(column);
    }

    /// Add a lookup argument for some input expressions and table columns.
    ///
    /// `table_map` returns a map between input expressions and the table columns
    /// they need to match.
    pub fn lookup<S: AsRef<str>>(
        &mut self,
        name: S,
        table_map: impl FnOnce(&mut VirtualCells<'_, F>) -> Vec<(Expression<F>, TableColumn)>,
    ) -> usize {
        let mut cells = VirtualCells::new(self);
        let table_map = table_map(&mut cells)
            .into_iter()
            .map(|(mut input, table)| {
                if input.contains_simple_selector() {
                    panic!("expression containing simple selector supplied to lookup argument");
                }
                let mut table = cells.query_fixed(table.inner(), Rotation::cur());
                input.query_cells(&mut cells);
                table.query_cells(&mut cells);
                (input, table)
            })
            .collect();
        let index = self.lookups.len();

        self.lookups
            .push(lookup::Argument::new(name.as_ref(), table_map));

        index
    }

    /// Add a lookup argument for some input expressions and table expressions.
    ///
    /// `table_map` returns a map between input expressions and the table expressions
    /// they need to match.
    pub fn lookup_any<S: AsRef<str>>(
        &mut self,
        name: S,
        table_map: impl FnOnce(&mut VirtualCells<'_, F>) -> Vec<(Expression<F>, Expression<F>)>,
    ) -> usize {
        let mut cells = VirtualCells::new(self);
        let table_map = table_map(&mut cells)
            .into_iter()
            .map(|(mut input, mut table)| {
                if input.contains_simple_selector() {
                    panic!("expression containing simple selector supplied to lookup argument");
                }
                if table.contains_simple_selector() {
                    panic!("expression containing simple selector supplied to lookup argument");
                }
                input.query_cells(&mut cells);
                table.query_cells(&mut cells);
                (input, table)
            })
            .collect();
        let index = self.lookups.len();

        self.lookups
            .push(lookup::Argument::new(name.as_ref(), table_map));

        index
    }

    /// Add a shuffle argument for some input expressions and table expressions.
    pub fn shuffle<S: AsRef<str>>(
        &mut self,
        name: S,
        shuffle_map: impl FnOnce(&mut VirtualCells<'_, F>) -> Vec<(Expression<F>, Expression<F>)>,
    ) -> usize {
        let mut cells = VirtualCells::new(self);
        let shuffle_map = shuffle_map(&mut cells)
            .into_iter()
            .map(|(mut input, mut table)| {
                input.query_cells(&mut cells);
                table.query_cells(&mut cells);
                (input, table)
            })
            .collect();
        let index = self.shuffles.len();

        self.shuffles
            .push(shuffle::Argument::new(name.as_ref(), shuffle_map));

        index
    }

    fn query_fixed_index(&mut self, column: Column<Fixed>, at: Rotation) -> usize {
        // Return existing query, if it exists
        for (index, fixed_query) in self.fixed_queries.iter().enumerate() {
            if fixed_query == &(column, at) {
                return index;
            }
        }

        // Make a new query
        let index = self.fixed_queries.len();
        self.fixed_queries.push((column, at));

        index
    }

    pub(crate) fn query_advice_index(&mut self, column: Column<Advice>, at: Rotation) -> usize {
        // Return existing query, if it exists
        for (index, advice_query) in self.advice_queries.iter().enumerate() {
            if advice_query == &(column, at) {
                return index;
            }
        }

        // Make a new query
        let index = self.advice_queries.len();
        self.advice_queries.push((column, at));
        self.num_advice_queries[column.index] += 1;

        index
    }

    fn query_instance_index(&mut self, column: Column<Instance>, at: Rotation) -> usize {
        // Return existing query, if it exists
        for (index, instance_query) in self.instance_queries.iter().enumerate() {
            if instance_query == &(column, at) {
                return index;
            }
        }

        // Make a new query
        let index = self.instance_queries.len();
        self.instance_queries.push((column, at));

        index
    }

    fn query_any_index(&mut self, column: Column<Any>, at: Rotation) -> usize {
        match column.column_type() {
            Any::Advice(_) => {
                self.query_advice_index(Column::<Advice>::try_from(column).unwrap(), at)
            }
            Any::Fixed => self.query_fixed_index(Column::<Fixed>::try_from(column).unwrap(), at),
            Any::Instance => {
                self.query_instance_index(Column::<Instance>::try_from(column).unwrap(), at)
            }
        }
    }

    pub(crate) fn get_advice_query_index(&self, column: Column<Advice>, at: Rotation) -> usize {
        for (index, advice_query) in self.advice_queries.iter().enumerate() {
            if advice_query == &(column, at) {
                return index;
            }
        }

        panic!("get_advice_query_index called for non-existent query");
    }

    pub(crate) fn get_fixed_query_index(&self, column: Column<Fixed>, at: Rotation) -> usize {
        for (index, fixed_query) in self.fixed_queries.iter().enumerate() {
            if fixed_query == &(column, at) {
                return index;
            }
        }

        panic!("get_fixed_query_index called for non-existent query");
    }

    pub(crate) fn get_instance_query_index(&self, column: Column<Instance>, at: Rotation) -> usize {
        for (index, instance_query) in self.instance_queries.iter().enumerate() {
            if instance_query == &(column, at) {
                return index;
            }
        }

        panic!("get_instance_query_index called for non-existent query");
    }

    pub fn get_any_query_index(&self, column: Column<Any>, at: Rotation) -> usize {
        match column.column_type() {
            Any::Advice(_) => {
                self.get_advice_query_index(Column::<Advice>::try_from(column).unwrap(), at)
            }
            Any::Fixed => {
                self.get_fixed_query_index(Column::<Fixed>::try_from(column).unwrap(), at)
            }
            Any::Instance => {
                self.get_instance_query_index(Column::<Instance>::try_from(column).unwrap(), at)
            }
        }
    }

    /// Sets the minimum degree required by the circuit, which can be set to a
    /// larger amount than actually needed. This can be used, for example, to
    /// force the permutation argument to involve more columns in the same set.
    pub fn set_minimum_degree(&mut self, degree: usize) {
        self.minimum_degree = Some(degree);
    }

    /// Creates a new gate.
    ///
    /// # Panics
    ///
    /// A gate is required to contain polynomial constraints. This method will panic if
    /// `constraints` returns an empty iterator.
    pub fn create_gate<C: Into<Constraint<F>>, Iter: IntoIterator<Item = C>, S: AsRef<str>>(
        &mut self,
        name: S,
        constraints: impl FnOnce(&mut VirtualCells<'_, F>) -> Iter,
    ) {
        let mut cells = VirtualCells::new(self);
        let constraints = constraints(&mut cells);
        let (constraint_names, polys): (_, Vec<_>) = constraints
            .into_iter()
            .map(|c| c.into())
            .map(|mut c: Constraint<F>| {
                c.poly.query_cells(&mut cells);
                (c.name, c.poly)
            })
            .unzip();

        let queried_selectors = cells.queried_selectors;
        let queried_cells = cells.queried_cells;

        assert!(
            !polys.is_empty(),
            "Gates must contain at least one constraint."
        );

        self.gates.push(Gate {
            name: name.as_ref().to_string(),
            constraint_names,
            polys,
            queried_selectors,
            queried_cells,
        });
    }

    /// This will compress selectors together depending on their provided
    /// assignments. This `ConstraintSystem` will then be modified to add new
    /// fixed columns (representing the actual selectors) and will return the
    /// polynomials for those columns. Finally, an internal map is updated to
    /// find which fixed column corresponds with a given `Selector`.
    ///
    /// Do not call this twice. Yes, this should be a builder pattern instead.
    pub fn compress_selectors(mut self, selectors: Vec<Vec<bool>>) -> (Self, Vec<Vec<F>>) {
        // The number of provided selector assignments must be the number we
        // counted for this constraint system.
        assert_eq!(selectors.len(), self.num_selectors);

        // Compute the maximal degree of every selector. We only consider the
        // expressions in gates, as lookup arguments cannot support simple
        // selectors. Selectors that are complex or do not appear in any gates
        // will have degree zero.
        let mut degrees = vec![0; selectors.len()];
        for expr in self.gates.iter().flat_map(|gate| gate.polys.iter()) {
            if let Some(selector) = expr.extract_simple_selector() {
                degrees[selector.0] = max(degrees[selector.0], expr.degree());
            }
        }

        // We will not increase the degree of the constraint system, so we limit
        // ourselves to the largest existing degree constraint.
        let max_degree = self.degree();

        let mut new_columns = vec![];
        let (polys, selector_assignment) = compress_selectors::process(
            selectors
                .into_iter()
                .zip(degrees)
                .enumerate()
                .map(
                    |(i, (activations, max_degree))| compress_selectors::SelectorDescription {
                        selector: i,
                        activations,
                        max_degree,
                    },
                )
                .collect(),
            max_degree,
            || {
                let column = self.fixed_column();
                new_columns.push(column);
                Expression::Fixed(FixedQuery {
                    index: Some(self.query_fixed_index(column, Rotation::cur())),
                    column_index: column.index,
                    rotation: Rotation::cur(),
                })
            },
        );

        let mut selector_map = vec![None; selector_assignment.len()];
        let mut selector_replacements = vec![None; selector_assignment.len()];
        for assignment in selector_assignment {
            selector_replacements[assignment.selector] = Some(assignment.expression);
            selector_map[assignment.selector] = Some(new_columns[assignment.combination_index]);
        }

        self.selector_map = selector_map
            .into_iter()
            .map(|a| a.unwrap())
            .collect::<Vec<_>>();
        let selector_replacements = selector_replacements
            .into_iter()
            .map(|a| a.unwrap())
            .collect::<Vec<_>>();
        self.replace_selectors_with_fixed(&selector_replacements);

        (self, polys)
    }

    /// Does not combine selectors and directly replaces them everywhere with fixed columns.
    pub fn directly_convert_selectors_to_fixed(
        mut self,
        selectors: Vec<Vec<bool>>,
    ) -> (Self, Vec<Vec<F>>) {
        // The number of provided selector assignments must be the number we
        // counted for this constraint system.
        assert_eq!(selectors.len(), self.num_selectors);

        let (polys, selector_replacements): (Vec<_>, Vec<_>) = selectors
            .into_iter()
            .map(|selector| {
                let poly = selector
                    .iter()
                    .map(|b| if *b { F::ONE } else { F::ZERO })
                    .collect::<Vec<_>>();
                let column = self.fixed_column();
                let rotation = Rotation::cur();
                let expr = Expression::Fixed(FixedQuery {
                    index: Some(self.query_fixed_index(column, rotation)),
                    column_index: column.index,
                    rotation,
                });
                (poly, expr)
            })
            .unzip();

        self.replace_selectors_with_fixed(&selector_replacements);
        self.num_selectors = 0;

        (self, polys)
    }

    fn replace_selectors_with_fixed(&mut self, selector_replacements: &[Expression<F>]) {
        fn replace_selectors<F: Field>(
            expr: &mut Expression<F>,
            selector_replacements: &[Expression<F>],
            must_be_nonsimple: bool,
        ) {
            *expr = expr.evaluate(
                &|constant| Expression::Constant(constant),
                &|selector| {
                    if must_be_nonsimple {
                        // Simple selectors are prohibited from appearing in
                        // expressions in the lookup argument by
                        // `ConstraintSystem`.
                        assert!(!selector.is_simple());
                    }

                    selector_replacements[selector.0].clone()
                },
                &|query| Expression::Fixed(query),
                &|query| Expression::Advice(query),
                &|query| Expression::Instance(query),
                &|challenge| Expression::Challenge(challenge),
                &|a| -a,
                &|a, b| a + b,
                &|a, b| a * b,
                &|a, f| a * f,
            );
        }

        // Substitute selectors for the real fixed columns in all gates
        for expr in self.gates.iter_mut().flat_map(|gate| gate.polys.iter_mut()) {
            replace_selectors(expr, selector_replacements, false);
        }

        // Substitute non-simple selectors for the real fixed columns in all
        // lookup expressions
        for expr in self.lookups.iter_mut().flat_map(|lookup| {
            lookup
                .input_expressions
                .iter_mut()
                .chain(lookup.table_expressions.iter_mut())
        }) {
            replace_selectors(expr, selector_replacements, true);
        }

        for expr in self.shuffles.iter_mut().flat_map(|shuffle| {
            shuffle
                .input_expressions
                .iter_mut()
                .chain(shuffle.shuffle_expressions.iter_mut())
        }) {
            replace_selectors(expr, selector_replacements, true);
        }
    }

    /// Allocate a new (simple) selector. Simple selectors cannot be added to
    /// expressions nor multiplied by other expressions containing simple
    /// selectors. Also, simple selectors may not appear in lookup argument
    /// inputs.
    pub fn selector(&mut self) -> Selector {
        let index = self.num_selectors;
        self.num_selectors += 1;
        Selector(index, true)
    }

    /// Allocate a new complex selector that can appear anywhere
    /// within expressions.
    pub fn complex_selector(&mut self) -> Selector {
        let index = self.num_selectors;
        self.num_selectors += 1;
        Selector(index, false)
    }

    /// Allocates a new fixed column that can be used in a lookup table.
    pub fn lookup_table_column(&mut self) -> TableColumn {
        TableColumn {
            inner: self.fixed_column(),
        }
    }

    /// Annotate a Lookup column.
    pub fn annotate_lookup_column<A, AR>(&mut self, column: TableColumn, annotation: A)
    where
        A: Fn() -> AR,
        AR: Into<String>,
    {
        // We don't care if the table has already an annotation. If it's the case we keep the new one.
        self.general_column_annotations.insert(
            metadata::Column::from((Any::Fixed, column.inner().index)),
            annotation().into(),
        );
    }

    /// Annotate an Instance column.
    pub fn annotate_lookup_any_column<A, AR, T>(&mut self, column: T, annotation: A)
    where
        A: Fn() -> AR,
        AR: Into<String>,
        T: Into<Column<Any>>,
    {
        let col_any = column.into();
        // We don't care if the table has already an annotation. If it's the case we keep the new one.
        self.general_column_annotations.insert(
            metadata::Column::from((col_any.column_type, col_any.index)),
            annotation().into(),
        );
    }

    /// Allocate a new fixed column
    pub fn fixed_column(&mut self) -> Column<Fixed> {
        let tmp = Column {
            index: self.num_fixed_columns,
            column_type: Fixed,
        };
        self.num_fixed_columns += 1;
        tmp
    }

    /// Allocate a new unblinded advice column at `FirstPhase`
    pub fn unblinded_advice_column(&mut self) -> Column<Advice> {
        self.unblinded_advice_column_in(FirstPhase)
    }

    /// Allocate a new advice column at `FirstPhase`
    pub fn advice_column(&mut self) -> Column<Advice> {
        self.advice_column_in(FirstPhase)
    }

    /// Allocate a new unblinded advice column in given phase. This allows for the generation of deterministic commitments to advice columns
    /// which can be used to split large circuits into smaller ones, whose proofs can then be "joined" together by their common witness commitments.
    pub fn unblinded_advice_column_in<P: Phase>(&mut self, phase: P) -> Column<Advice> {
        let phase = phase.to_sealed();
        if let Some(previous_phase) = phase.prev() {
            self.assert_phase_exists(
                previous_phase,
                format!("Column<Advice> in later phase {phase:?}").as_str(),
            );
        }

        let tmp = Column {
            index: self.num_advice_columns,
            column_type: Advice { phase: phase.0 },
        };
        self.unblinded_advice_columns.push(tmp.index);
        self.num_advice_columns += 1;
        self.num_advice_queries.push(0);
        self.advice_column_phase.push(phase);
        tmp
    }

    /// Allocate a new advice column in given phase
    ///
    /// # Panics
    ///
    /// It panics if previous phase before the given one doesn't have advice column allocated.
    pub fn advice_column_in<P: Phase>(&mut self, phase: P) -> Column<Advice> {
        let phase = phase.to_sealed();
        if let Some(previous_phase) = phase.prev() {
            self.assert_phase_exists(
                previous_phase,
                format!("Column<Advice> in later phase {phase:?}").as_str(),
            );
        }

        let tmp = Column {
            index: self.num_advice_columns,
            column_type: Advice { phase: phase.0 },
        };
        self.num_advice_columns += 1;
        self.num_advice_queries.push(0);
        self.advice_column_phase.push(phase);
        tmp
    }

    /// Allocate a new instance column
    pub fn instance_column(&mut self) -> Column<Instance> {
        let tmp = Column {
            index: self.num_instance_columns,
            column_type: Instance,
        };
        self.num_instance_columns += 1;
        tmp
    }

    /// Requests a challenge that is usable after the given phase.
    ///
    /// # Panics
    ///
    /// It panics if the given phase doesn't have advice column allocated.
    pub fn challenge_usable_after<P: Phase>(&mut self, phase: P) -> Challenge {
        let phase = phase.to_sealed();
        self.assert_phase_exists(
            phase,
            format!("Challenge usable after phase {phase:?}").as_str(),
        );

        let tmp = Challenge {
            index: self.num_challenges,
            phase: phase.0,
        };
        self.num_challenges += 1;
        self.challenge_phase.push(phase);
        tmp
    }

    /// Helper funciotn to assert phase exists, to make sure phase-aware resources
    /// are allocated in order, and to avoid any phase to be skipped accidentally
    /// to cause unexpected issue in the future.
    fn assert_phase_exists(&self, phase: sealed::Phase, resource: &str) {
        self.advice_column_phase
            .iter()
            .find(|advice_column_phase| **advice_column_phase == phase)
            .unwrap_or_else(|| {
                panic!(
                    "No Column<Advice> is used in phase {phase:?} while allocating a new {resource:?}"
                )
            });
    }

    /// Returns the list of phases
    pub fn phases(&self) -> impl Iterator<Item = sealed::Phase> {
        let max_phase = self
            .advice_column_phase
            .iter()
            .max()
            .map(|phase| phase.0)
            .unwrap_or_default();
        (0..=max_phase).map(sealed::Phase)
    }

    /// Compute the degree of the constraint system (the maximum degree of all
    /// constraints).
    pub fn degree(&self) -> usize {
        // The permutation argument will serve alongside the gates, so must be
        // accounted for.
        let mut degree = self.permutation.required_degree();

        // The lookup argument also serves alongside the gates and must be accounted
        // for.
        degree = std::cmp::max(
            degree,
            self.lookups
                .iter()
                .map(|l| l.required_degree())
                .max()
                .unwrap_or(1),
        );

        // The lookup argument also serves alongside the gates and must be accounted
        // for.
        degree = std::cmp::max(
            degree,
            self.shuffles
                .iter()
                .map(|l| l.required_degree())
                .max()
                .unwrap_or(1),
        );

        // Account for each gate to ensure our quotient polynomial is the
        // correct degree and that our extended domain is the right size.
        degree = std::cmp::max(
            degree,
            self.gates
                .iter()
                .flat_map(|gate| gate.polynomials().iter().map(|poly| poly.degree()))
                .max()
                .unwrap_or(0),
        );

        std::cmp::max(degree, self.minimum_degree.unwrap_or(1))
    }

    /// Compute the number of blinding factors necessary to perfectly blind
    /// each of the prover's witness polynomials.
    pub fn blinding_factors(&self) -> usize {
        // All of the prover's advice columns are evaluated at no more than
        let factors = *self.num_advice_queries.iter().max().unwrap_or(&1);
        // distinct points during gate checks.

        // - The permutation argument witness polynomials are evaluated at most 3 times.
        // - Each lookup argument has independent witness polynomials, and they are
        //   evaluated at most 2 times.
        let factors = std::cmp::max(3, factors);

        // Each polynomial is evaluated at most an additional time during
        // multiopen (at x_3 to produce q_evals):
        let factors = factors + 1;

        // h(x) is derived by the other evaluations so it does not reveal
        // anything; in fact it does not even appear in the proof.

        // h(x_3) is also not revealed; the verifier only learns a single
        // evaluation of a polynomial in x_1 which has h(x_3) and another random
        // polynomial evaluated at x_3 as coefficients -- this random polynomial
        // is "random_poly" in the vanishing argument.

        // Add an additional blinding factor as a slight defense against
        // off-by-one errors.
        factors + 1
    }

    /// Returns the minimum necessary rows that need to exist in order to
    /// account for e.g. blinding factors.
    pub fn minimum_rows(&self) -> usize {
        self.blinding_factors() // m blinding factors
            + 1 // for l_{-(m + 1)} (l_last)
            + 1 // for l_0 (just for extra breathing room for the permutation
                // argument, to essentially force a separation in the
                // permutation polynomial between the roles of l_last, l_0
                // and the interstitial values.)
            + 1 // for at least one row
    }

    /// Returns number of fixed columns
    pub fn num_fixed_columns(&self) -> usize {
        self.num_fixed_columns
    }

    /// Returns number of advice columns
    pub fn num_advice_columns(&self) -> usize {
        self.num_advice_columns
    }

    /// Returns number of instance columns
    pub fn num_instance_columns(&self) -> usize {
        self.num_instance_columns
    }

    /// Returns number of selectors
    pub fn num_selectors(&self) -> usize {
        self.num_selectors
    }

    /// Returns number of challenges
    pub fn num_challenges(&self) -> usize {
        self.num_challenges
    }

    /// Returns phase of advice columns
    pub fn advice_column_phase(&self) -> Vec<u8> {
        self.advice_column_phase
            .iter()
            .map(|phase| phase.0)
            .collect()
    }

    /// Returns phase of challenges
    pub fn challenge_phase(&self) -> Vec<u8> {
        self.challenge_phase.iter().map(|phase| phase.0).collect()
    }

    /// Returns gates
    pub fn gates(&self) -> &Vec<Gate<F>> {
        &self.gates
    }

    /// Returns general column annotations
    pub fn general_column_annotations(&self) -> &HashMap<metadata::Column, String> {
        &self.general_column_annotations
    }

    /// Returns advice queries
    pub fn advice_queries(&self) -> &Vec<(Column<Advice>, Rotation)> {
        &self.advice_queries
    }

    /// Returns instance queries
    pub fn instance_queries(&self) -> &Vec<(Column<Instance>, Rotation)> {
        &self.instance_queries
    }

    /// Returns fixed queries
    pub fn fixed_queries(&self) -> &Vec<(Column<Fixed>, Rotation)> {
        &self.fixed_queries
    }

    /// Returns permutation argument
    pub fn permutation(&self) -> &permutation::Argument {
        &self.permutation
    }

    /// Returns lookup arguments
    pub fn lookups(&self) -> &Vec<lookup::Argument<F>> {
        &self.lookups
    }

    /// Returns shuffle arguments
    pub fn shuffles(&self) -> &Vec<shuffle::Argument<F>> {
        &self.shuffles
    }

    /// Returns constants
    pub fn constants(&self) -> &Vec<Column<Fixed>> {
        &self.constants
    }
}

/// Exposes the "virtual cells" that can be queried while creating a custom gate or lookup
/// table.
#[derive(Debug)]
pub struct VirtualCells<'a, F: Field> {
    meta: &'a mut ConstraintSystem<F>,
    queried_selectors: Vec<Selector>,
    queried_cells: Vec<VirtualCell>,
}

impl<'a, F: Field> VirtualCells<'a, F> {
    fn new(meta: &'a mut ConstraintSystem<F>) -> Self {
        VirtualCells {
            meta,
            queried_selectors: vec![],
            queried_cells: vec![],
        }
    }

    /// Query a selector at the current position.
    pub fn query_selector(&mut self, selector: Selector) -> Expression<F> {
        self.queried_selectors.push(selector);
        Expression::Selector(selector)
    }

    /// Query a fixed column at a relative position
    pub fn query_fixed(&mut self, column: Column<Fixed>, at: Rotation) -> Expression<F> {
        self.queried_cells.push((column, at).into());
        Expression::Fixed(FixedQuery {
            index: Some(self.meta.query_fixed_index(column, at)),
            column_index: column.index,
            rotation: at,
        })
    }

    /// Query an advice column at a relative position
    pub fn query_advice(&mut self, column: Column<Advice>, at: Rotation) -> Expression<F> {
        self.queried_cells.push((column, at).into());
        Expression::Advice(AdviceQuery {
            index: Some(self.meta.query_advice_index(column, at)),
            column_index: column.index,
            rotation: at,
            phase: sealed::Phase(column.column_type().phase),
        })
    }

    /// Query an instance column at a relative position
    pub fn query_instance(&mut self, column: Column<Instance>, at: Rotation) -> Expression<F> {
        self.queried_cells.push((column, at).into());
        Expression::Instance(InstanceQuery {
            index: Some(self.meta.query_instance_index(column, at)),
            column_index: column.index,
            rotation: at,
        })
    }

    /// Query an Any column at a relative position
    pub fn query_any<C: Into<Column<Any>>>(&mut self, column: C, at: Rotation) -> Expression<F> {
        let column = column.into();
        match column.column_type() {
            Any::Advice(_) => self.query_advice(Column::<Advice>::try_from(column).unwrap(), at),
            Any::Fixed => self.query_fixed(Column::<Fixed>::try_from(column).unwrap(), at),
            Any::Instance => self.query_instance(Column::<Instance>::try_from(column).unwrap(), at),
        }
    }

    /// Query a challenge
    pub fn query_challenge(&mut self, challenge: Challenge) -> Expression<F> {
        Expression::Challenge(challenge)
    }
}

#[cfg(test)]
mod tests {
    use super::Expression;
    use halo2curves::bn256::Fr;

    #[test]
    fn iter_sum() {
        let exprs: Vec<Expression<Fr>> = vec![
            Expression::Constant(1.into()),
            Expression::Constant(2.into()),
            Expression::Constant(3.into()),
        ];
        let happened: Expression<Fr> = exprs.into_iter().sum();
        let expected: Expression<Fr> = Expression::Sum(
            Box::new(Expression::Sum(
                Box::new(Expression::Constant(1.into())),
                Box::new(Expression::Constant(2.into())),
            )),
            Box::new(Expression::Constant(3.into())),
        );

        assert_eq!(happened, expected);
    }

    #[test]
    fn iter_product() {
        let exprs: Vec<Expression<Fr>> = vec![
            Expression::Constant(1.into()),
            Expression::Constant(2.into()),
            Expression::Constant(3.into()),
        ];
        let happened: Expression<Fr> = exprs.into_iter().product();
        let expected: Expression<Fr> = Expression::Product(
            Box::new(Expression::Product(
                Box::new(Expression::Constant(1.into())),
                Box::new(Expression::Constant(2.into())),
            )),
            Box::new(Expression::Constant(3.into())),
        );

        assert_eq!(happened, expected);
    }
}
