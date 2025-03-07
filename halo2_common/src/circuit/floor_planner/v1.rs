use std::fmt;

use halo2_middleware::ff::Field;

use crate::plonk::Assigned;
use crate::{
    circuit::{
        layouter::{RegionColumn, RegionLayouter, RegionShape, SyncDeps, TableLayouter},
        table_layouter::{compute_table_lengths, SimpleTableLayouter},
        Cell, Column, Layouter, Region, RegionIndex, RegionStart, Table, Value,
    },
    plonk::{circuit::Challenge, Assignment, Circuit, Error, FloorPlanner, Selector, TableColumn},
};
use halo2_middleware::circuit::{Advice, Any, Fixed, Instance};

pub mod strategy;

/// The version 1 [`FloorPlanner`] provided by `halo2`.
///
/// - No column optimizations are performed. Circuit configuration is left entirely to the
///   circuit designer.
/// - A dual-pass layouter is used to measures regions prior to assignment.
/// - Regions are measured as rectangles, bounded on the cells they assign.
/// - Regions are laid out using a greedy first-fit strategy, after sorting regions by
///   their "advice area" (number of advice columns * rows).
#[derive(Debug)]
pub struct V1;

struct V1Plan<'a, F: Field, CS: Assignment<F> + 'a> {
    cs: &'a mut CS,
    /// Stores the starting row for each region.
    regions: Vec<RegionStart>,
    /// Stores the constants to be assigned, and the cells to which they are copied.
    constants: Vec<(Assigned<F>, Cell)>,
    /// Stores the table fixed columns.
    table_columns: Vec<TableColumn>,
}

impl<'a, F: Field, CS: Assignment<F> + 'a> fmt::Debug for V1Plan<'a, F, CS> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("floor_planner::V1Plan").finish()
    }
}

impl<'a, F: Field, CS: Assignment<F> + SyncDeps> V1Plan<'a, F, CS> {
    /// Creates a new v1 layouter.
    pub fn new(cs: &'a mut CS) -> Result<Self, Error> {
        let ret = V1Plan {
            cs,
            regions: vec![],
            constants: vec![],
            table_columns: vec![],
        };
        Ok(ret)
    }
}

impl FloorPlanner for V1 {
    fn synthesize<F: Field, CS: Assignment<F> + SyncDeps, C: Circuit<F>>(
        cs: &mut CS,
        circuit: &C,
        config: C::Config,
        constants: Vec<Column<Fixed>>,
    ) -> Result<(), Error> {
        let mut plan = V1Plan::new(cs)?;

        // First pass: measure the regions within the circuit.
        let mut measure = MeasurementPass::new();
        {
            let pass = &mut measure;
            circuit
                .without_witnesses()
                .synthesize(config.clone(), V1Pass::<_, CS>::measure(pass))?;
        }

        // Planning:
        // - Position the regions.
        let (regions, column_allocations) = strategy::slot_in_biggest_advice_first(measure.regions);
        plan.regions = regions;

        // - Determine how many rows our planned circuit will require.
        let first_unassigned_row = column_allocations
            .values()
            .map(|a| a.unbounded_interval_start())
            .max()
            .unwrap_or(0);

        // - Position the constants within those rows.
        let fixed_allocations: Vec<_> = constants
            .into_iter()
            .map(|c| {
                (
                    c,
                    column_allocations
                        .get(&Column::<Any>::from(c).into())
                        .cloned()
                        .unwrap_or_default(),
                )
            })
            .collect();
        let constant_positions = || {
            fixed_allocations.iter().flat_map(|(c, a)| {
                let c = *c;
                a.free_intervals(0, Some(first_unassigned_row))
                    .flat_map(move |e| e.range().unwrap().map(move |i| (c, i)))
            })
        };

        // Second pass:
        // - Assign the regions.
        let mut assign = AssignmentPass::new(&mut plan);
        {
            let pass = &mut assign;
            circuit.synthesize(config, V1Pass::assign(pass))?;
        }

        // - Assign the constants.
        if constant_positions().count() < plan.constants.len() {
            return Err(Error::NotEnoughColumnsForConstants);
        }
        for ((fixed_column, fixed_row), (value, advice)) in
            constant_positions().zip(plan.constants.into_iter())
        {
            plan.cs.assign_fixed(
                || format!("Constant({:?})", value.evaluate()),
                fixed_column,
                fixed_row,
                || Value::known(value),
            )?;
            plan.cs.copy(
                fixed_column.into(),
                fixed_row,
                advice.column,
                *plan.regions[*advice.region_index] + advice.row_offset,
            )?;
        }

        Ok(())
    }
}

#[derive(Debug)]
enum Pass<'p, 'a, F: Field, CS: Assignment<F> + 'a> {
    Measurement(&'p mut MeasurementPass),
    Assignment(&'p mut AssignmentPass<'p, 'a, F, CS>),
}

/// A single pass of the [`V1`] layouter.
#[derive(Debug)]
pub struct V1Pass<'p, 'a, F: Field, CS: Assignment<F> + 'a>(Pass<'p, 'a, F, CS>);

impl<'p, 'a, F: Field, CS: Assignment<F> + 'a> V1Pass<'p, 'a, F, CS> {
    fn measure(pass: &'p mut MeasurementPass) -> Self {
        V1Pass(Pass::Measurement(pass))
    }

    fn assign(pass: &'p mut AssignmentPass<'p, 'a, F, CS>) -> Self {
        V1Pass(Pass::Assignment(pass))
    }
}

impl<'p, 'a, F: Field, CS: Assignment<F> + SyncDeps> Layouter<F> for V1Pass<'p, 'a, F, CS> {
    type Root = Self;

    fn assign_region<A, AR, N, NR>(&mut self, name: N, assignment: A) -> Result<AR, Error>
    where
        A: FnMut(Region<'_, F>) -> Result<AR, Error>,
        N: Fn() -> NR,
        NR: Into<String>,
    {
        match &mut self.0 {
            Pass::Measurement(pass) => pass.assign_region(assignment),
            Pass::Assignment(pass) => pass.assign_region(name, assignment),
        }
    }

    fn assign_table<A, N, NR>(&mut self, name: N, assignment: A) -> Result<(), Error>
    where
        A: FnMut(Table<'_, F>) -> Result<(), Error>,
        N: Fn() -> NR,
        NR: Into<String>,
    {
        match &mut self.0 {
            Pass::Measurement(_) => Ok(()),
            Pass::Assignment(pass) => pass.assign_table(name, assignment),
        }
    }

    fn constrain_instance(
        &mut self,
        cell: Cell,
        instance: Column<Instance>,
        row: usize,
    ) -> Result<(), Error> {
        match &mut self.0 {
            Pass::Measurement(_) => Ok(()),
            Pass::Assignment(pass) => pass.constrain_instance(cell, instance, row),
        }
    }

    fn get_challenge(&self, challenge: Challenge) -> Value<F> {
        match &self.0 {
            Pass::Measurement(_) => Value::unknown(),
            Pass::Assignment(pass) => pass.plan.cs.get_challenge(challenge),
        }
    }

    fn get_root(&mut self) -> &mut Self::Root {
        self
    }

    fn push_namespace<NR, N>(&mut self, name_fn: N)
    where
        NR: Into<String>,
        N: FnOnce() -> NR,
    {
        if let Pass::Assignment(pass) = &mut self.0 {
            pass.plan.cs.push_namespace(name_fn);
        }
    }

    fn pop_namespace(&mut self, gadget_name: Option<String>) {
        if let Pass::Assignment(pass) = &mut self.0 {
            pass.plan.cs.pop_namespace(gadget_name);
        }
    }
}

/// Measures the circuit.
#[derive(Debug)]
pub struct MeasurementPass {
    regions: Vec<RegionShape>,
}

impl MeasurementPass {
    fn new() -> Self {
        MeasurementPass { regions: vec![] }
    }

    fn assign_region<F: Field, A, AR>(&mut self, mut assignment: A) -> Result<AR, Error>
    where
        A: FnMut(Region<'_, F>) -> Result<AR, Error>,
    {
        let region_index = self.regions.len();

        // Get shape of the region.
        let mut shape = RegionShape::new(region_index.into());
        let result = {
            let region: &mut dyn RegionLayouter<F> = &mut shape;
            assignment(region.into())
        }?;
        self.regions.push(shape);

        Ok(result)
    }
}

/// Assigns the circuit.
#[derive(Debug)]
pub struct AssignmentPass<'p, 'a, F: Field, CS: Assignment<F> + 'a> {
    plan: &'p mut V1Plan<'a, F, CS>,
    /// Counter tracking which region we need to assign next.
    region_index: usize,
}

impl<'p, 'a, F: Field, CS: Assignment<F> + SyncDeps> AssignmentPass<'p, 'a, F, CS> {
    fn new(plan: &'p mut V1Plan<'a, F, CS>) -> Self {
        AssignmentPass {
            plan,
            region_index: 0,
        }
    }

    fn assign_region<A, AR, N, NR>(&mut self, name: N, mut assignment: A) -> Result<AR, Error>
    where
        A: FnMut(Region<'_, F>) -> Result<AR, Error>,
        N: Fn() -> NR,
        NR: Into<String>,
    {
        // Get the next region we are assigning.
        let region_index = self.region_index;
        self.region_index += 1;

        self.plan.cs.enter_region(name);
        let mut region = V1Region::new(self.plan, region_index.into());
        let result = {
            let region: &mut dyn RegionLayouter<F> = &mut region;
            assignment(region.into())
        }?;
        self.plan.cs.exit_region();

        Ok(result)
    }

    fn assign_table<A, AR, N, NR>(&mut self, name: N, mut assignment: A) -> Result<AR, Error>
    where
        A: FnMut(Table<'_, F>) -> Result<AR, Error>,
        N: Fn() -> NR,
        NR: Into<String>,
    {
        // Maintenance hazard: there is near-duplicate code in `SingleChipLayouter::assign_table`.

        // Assign table cells.
        self.plan.cs.enter_region(name);
        let mut table = SimpleTableLayouter::new(self.plan.cs, &self.plan.table_columns);
        let result = {
            let table: &mut dyn TableLayouter<F> = &mut table;
            assignment(table.into())
        }?;
        let default_and_assigned = table.default_and_assigned;
        self.plan.cs.exit_region();

        // Check that all table columns have the same length `first_unused`,
        // and all cells up to that length are assigned.
        let first_unused = compute_table_lengths(&default_and_assigned)?;

        // Record these columns so that we can prevent them from being used again.
        for column in default_and_assigned.keys() {
            self.plan.table_columns.push(*column);
        }

        for (col, (default_val, _)) in default_and_assigned {
            // default_val must be Some because we must have assigned
            // at least one cell in each column, and in that case we checked
            // that all cells up to first_unused were assigned.
            self.plan
                .cs
                .fill_from_row(col.inner(), first_unused, default_val.unwrap())?;
        }

        Ok(result)
    }

    fn constrain_instance(
        &mut self,
        cell: Cell,
        instance: Column<Instance>,
        row: usize,
    ) -> Result<(), Error> {
        self.plan.cs.copy(
            cell.column,
            *self.plan.regions[*cell.region_index] + cell.row_offset,
            instance.into(),
            row,
        )
    }
}

struct V1Region<'r, 'a, F: Field, CS: Assignment<F> + 'a> {
    plan: &'r mut V1Plan<'a, F, CS>,
    region_index: RegionIndex,
}

impl<'r, 'a, F: Field, CS: Assignment<F> + 'a> fmt::Debug for V1Region<'r, 'a, F, CS> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("V1Region")
            .field("plan", &self.plan)
            .field("region_index", &self.region_index)
            .finish()
    }
}

impl<'r, 'a, F: Field, CS: Assignment<F> + 'a> V1Region<'r, 'a, F, CS> {
    fn new(plan: &'r mut V1Plan<'a, F, CS>, region_index: RegionIndex) -> Self {
        V1Region { plan, region_index }
    }
}

impl<'r, 'a, F: Field, CS: Assignment<F> + SyncDeps> RegionLayouter<F> for V1Region<'r, 'a, F, CS> {
    fn enable_selector<'v>(
        &'v mut self,
        annotation: &'v (dyn Fn() -> String + 'v),
        selector: &Selector,
        offset: usize,
    ) -> Result<(), Error> {
        self.plan.cs.enable_selector(
            annotation,
            selector,
            *self.plan.regions[*self.region_index] + offset,
        )
    }

    fn assign_advice<'v>(
        &'v mut self,
        annotation: &'v (dyn Fn() -> String + 'v),
        column: Column<Advice>,
        offset: usize,
        to: &'v mut (dyn FnMut() -> Value<Assigned<F>> + 'v),
    ) -> Result<Cell, Error> {
        self.plan.cs.assign_advice(
            annotation,
            column,
            *self.plan.regions[*self.region_index] + offset,
            to,
        )?;

        Ok(Cell {
            region_index: self.region_index,
            row_offset: offset,
            column: column.into(),
        })
    }

    fn assign_advice_from_constant<'v>(
        &'v mut self,
        annotation: &'v (dyn Fn() -> String + 'v),
        column: Column<Advice>,
        offset: usize,
        constant: Assigned<F>,
    ) -> Result<Cell, Error> {
        let advice =
            self.assign_advice(annotation, column, offset, &mut || Value::known(constant))?;
        self.constrain_constant(advice, constant)?;

        Ok(advice)
    }

    fn assign_advice_from_instance<'v>(
        &mut self,
        annotation: &'v (dyn Fn() -> String + 'v),
        instance: Column<Instance>,
        row: usize,
        advice: Column<Advice>,
        offset: usize,
    ) -> Result<(Cell, Value<F>), Error> {
        let value = self.plan.cs.query_instance(instance, row)?;

        let cell = self.assign_advice(annotation, advice, offset, &mut || value.to_field())?;

        self.plan.cs.copy(
            cell.column,
            *self.plan.regions[*cell.region_index] + cell.row_offset,
            instance.into(),
            row,
        )?;

        Ok((cell, value))
    }

    fn instance_value(
        &mut self,
        instance: Column<Instance>,
        row: usize,
    ) -> Result<Value<F>, Error> {
        self.plan.cs.query_instance(instance, row)
    }

    fn assign_fixed<'v>(
        &'v mut self,
        annotation: &'v (dyn Fn() -> String + 'v),
        column: Column<Fixed>,
        offset: usize,
        to: &'v mut (dyn FnMut() -> Value<Assigned<F>> + 'v),
    ) -> Result<Cell, Error> {
        self.plan.cs.assign_fixed(
            annotation,
            column,
            *self.plan.regions[*self.region_index] + offset,
            to,
        )?;

        Ok(Cell {
            region_index: self.region_index,
            row_offset: offset,
            column: column.into(),
        })
    }

    fn constrain_constant(&mut self, cell: Cell, constant: Assigned<F>) -> Result<(), Error> {
        self.plan.constants.push((constant, cell));
        Ok(())
    }

    fn name_column<'v>(
        &'v mut self,
        annotation: &'v (dyn Fn() -> String + 'v),
        column: Column<Any>,
    ) {
        self.plan.cs.annotate_column(annotation, column)
    }

    fn constrain_equal(&mut self, left: Cell, right: Cell) -> Result<(), Error> {
        self.plan.cs.copy(
            left.column,
            *self.plan.regions[*left.region_index] + left.row_offset,
            right.column,
            *self.plan.regions[*right.region_index] + right.row_offset,
        )?;

        Ok(())
    }
}
