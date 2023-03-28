use std::{collections::HashSet, convert::TryInto, mem::discriminant};

use super::{validate_for_array_assignment, ValidationContext, Validator, Validators};
use crate::{
    ast::{self, AstStatement, ConditionalBlock, DirectAccessType, Operator, SourceRange},
    codegen::generators::expression_generator::get_implicit_call_parameter,
    index::{ArgumentType, Index, PouIndexEntry, VariableIndexEntry, VariableType},
    resolver::{const_evaluator, AnnotationMap, StatementAnnotation},
    typesystem::{
        self, get_equals_function_name_for, DataType, DataTypeInformation, Dimension, BOOL_TYPE, POINTER_SIZE,
    },
    Diagnostic,
};

macro_rules! visit_all_statements {
    ($validator:expr, $context:expr, $last:expr ) => {
        visit_statement($validator, $last, $context);
    };

    ($validator:expr, $context:expr, $head:expr, $($tail:expr), +) => {
      visit_statement($validator, $head, $context);
      visit_all_statements!($validator, $context, $($tail),+)
    };
}

pub fn visit_statement(validator: &mut Validator, statement: &AstStatement, context: &ValidationContext) {
    match statement {
        // AstStatement::EmptyStatement { location, id } => (),
        // AstStatement::DefaultValue { location, id } => (),
        // AstStatement::LiteralInteger { value, location, id } => (),
        // AstStatement::LiteralDate { year, month, day, location, id } => (),
        // AstStatement::LiteralDateAndTime { year, month, day, hour, min, sec, nano, location, id } => (),
        // AstStatement::LiteralTimeOfDay { hour, min, sec, nano, location, id } => (),
        // AstStatement::LiteralTime { day, hour, min, sec, milli, micro, nano, negative, location, id } => (),
        // AstStatement::LiteralReal { value, location, id } => (),
        // AstStatement::LiteralBool { value, location, id } => (),
        // AstStatement::LiteralString { value, is_wide, location, id } => (),
        AstStatement::LiteralArray { elements: Some(elements), .. } => {
            visit_statement(validator, elements.as_ref(), context);
        }
        AstStatement::CastStatement { target, type_name, location, .. } => {
            validate_cast_literal(validator, target, type_name, location, context);
        }
        AstStatement::MultipliedStatement { element, .. } => {
            visit_statement(validator, element, context);
        }
        AstStatement::QualifiedReference { elements, .. } => {
            elements.iter().for_each(|element| visit_statement(validator, element, context));
            validate_qualified_reference(validator, elements, context);
        }
        AstStatement::Reference { name, location, .. } => {
            validate_reference(validator, statement, name, location, context);
        }
        AstStatement::ArrayAccess { reference, access, .. } => {
            visit_all_statements!(validator, context, reference, access);
            visit_array_access(validator, reference, access, context);
        }
        // AstStatement::PointerAccess { reference, id } => (),
        // AstStatement::DirectAccess { access, index, location, id } => (),
        // AstStatement::HardwareAccess { direction, access, address, location, id } => (),
        AstStatement::BinaryExpression { operator, left, right, .. } => {
            visit_all_statements!(validator, context, left, right);
            visit_binary_expression(validator, statement, operator, left, right, context);
        }
        AstStatement::UnaryExpression { operator, value, location, .. } => {
            visit_statement(validator, value, context);
            validate_unary_expression(validator, operator, value, location);
        }
        AstStatement::ExpressionList { expressions, .. } => {
            validate_for_array_assignment(validator, expressions, context);
            expressions.iter().for_each(|element| visit_statement(validator, element, context))
        }
        AstStatement::RangeStatement { start, end, .. } => {
            visit_all_statements!(validator, context, start, end);
        }
        AstStatement::Assignment { left, right, .. } => {
            visit_statement(validator, left, context);
            visit_statement(validator, right, context);

            validate_assignment(validator, right, Some(left), &statement.get_location(), context);
        }
        AstStatement::OutputAssignment { left, right, .. } => {
            visit_statement(validator, left, context);
            visit_statement(validator, right, context);

            validate_assignment(validator, right, Some(left), &statement.get_location(), context);
        }
        AstStatement::CallStatement { operator, parameters, .. } => {
            validate_call(validator, operator, parameters, context);
        }
        AstStatement::IfStatement { blocks, else_block, .. } => {
            blocks.iter().for_each(|b| {
                visit_statement(validator, b.condition.as_ref(), context);
                b.body.iter().for_each(|s| visit_statement(validator, s, context));
            });
            else_block.iter().for_each(|e| visit_statement(validator, e, context));
        }
        AstStatement::ForLoopStatement { counter, start, end, by_step, body, .. } => {
            visit_all_statements!(validator, context, counter, start, end);
            if let Some(by_step) = by_step {
                visit_statement(validator, by_step, context);
            }
            body.iter().for_each(|s| visit_statement(validator, s, context));
        }
        AstStatement::WhileLoopStatement { condition, body, .. } => {
            visit_statement(validator, condition, context);
            body.iter().for_each(|s| visit_statement(validator, s, context));
        }
        AstStatement::RepeatLoopStatement { condition, body, .. } => {
            visit_statement(validator, condition, context);
            body.iter().for_each(|s| visit_statement(validator, s, context));
        }
        AstStatement::CaseStatement { selector, case_blocks, else_block, .. } => {
            validate_case_statement(validator, selector, case_blocks, else_block, context);
        }
        AstStatement::CaseCondition { condition, .. } => {
            // if we get here, then a `CaseCondition` is used outside a `CaseStatement`
            // `CaseCondition` are used as a marker for `CaseStatements` and are not passed as such to the `CaseStatement.case_blocks`
            // see `control_parser` `parse_case_statement()`
            validator.push_diagnostic(Diagnostic::case_condition_used_outside_case_statement(
                condition.get_location(),
            ));
            visit_statement(validator, condition, context);
        }
        // AstStatement::ExitStatement { location, id } => (),
        // AstStatement::ContinueStatement { location, id } => (),
        // AstStatement::ReturnStatement { location, id } => (),
        // AstStatement::LiteralNull { location, id } => (),
        _ => {}
    }
    validate_type_nature(validator, statement, context);
}

/// validates a literal statement with a dedicated type-prefix (e.g. INT#3)
/// checks whether the type-prefix is valid
fn validate_cast_literal(
    // TODO: i feel like literal is misleading here. can be a reference aswell (INT#x)
    validator: &mut Validator,
    literal: &AstStatement,
    type_name: &str,
    location: &SourceRange,
    context: &ValidationContext,
) {
    let cast_type = context.index.get_effective_type_or_void_by_name(type_name).get_type_information();
    let literal_type = context.index.get_type_information_or_void(
        literal
            .get_literal_actual_signed_type_name(!cast_type.is_unsigned_int())
            .or_else(|| context.annotations.get_type_hint(literal, context.index).map(DataType::get_name))
            .unwrap_or_else(|| context.annotations.get_type_or_void(literal, context.index).get_name()),
    );

    if !literal.is_cast_prefix_eligible() {
        validator.push_diagnostic(Diagnostic::literal_expected(location.clone()))
    } else if cast_type.is_date_or_time_type() || literal_type.is_date_or_time_type() {
        validator.push_diagnostic(Diagnostic::incompatible_literal_cast(
            cast_type.get_name(),
            literal_type.get_name(),
            location.clone(),
        ));
        // see if target and cast_type are compatible
    } else if cast_type.is_int() && literal_type.is_int() {
        // INTs with INTs
        if cast_type.get_semantic_size(context.index) < literal_type.get_semantic_size(context.index) {
            validator.push_diagnostic(Diagnostic::literal_out_of_range(
                literal.get_literal_value().as_str(),
                cast_type.get_name(),
                location.clone(),
            ));
        }
    } else if cast_type.is_character() && literal_type.is_string() {
        let value = literal.get_literal_value();
        // value contains "" / ''
        if value.len() > 3 {
            validator.push_diagnostic(Diagnostic::literal_out_of_range(
                value.as_str(),
                cast_type.get_name(),
                location.clone(),
            ));
        }
    } else if discriminant(cast_type) != discriminant(literal_type) {
        // different types
        // REAL#100 is fine, other differences are not
        if !(cast_type.is_float() && literal_type.is_int()) {
            validator.push_diagnostic(Diagnostic::incompatible_literal_cast(
                cast_type.get_name(),
                literal.get_literal_value().as_str(),
                location.clone(),
            ));
        }
    }
}

fn validate_qualified_reference(
    validator: &mut Validator,
    elements: &[AstStatement],
    context: &ValidationContext,
) {
    let mut iter = elements.iter().rev();
    if let Some((AstStatement::DirectAccess { access, index, location, .. }, reference)) =
        iter.next().zip(iter.next())
    {
        let target_type =
            context.annotations.get_type_or_void(reference, context.index).get_type_information();
        if target_type.is_int() {
            if !access.is_compatible(target_type, context.index) {
                validator.push_diagnostic(Diagnostic::incompatible_directaccess(
                    &format!("{access:?}"),
                    access.get_bit_width(),
                    location.clone(),
                ))
            } else {
                validate_access_index(validator, context, index, access, target_type, location);
            }
        } else {
            validator.push_diagnostic(Diagnostic::incompatible_directaccess(
                &format!("{access:?}"),
                access.get_bit_width(),
                location.clone(),
            ))
        }
    }
}

fn validate_access_index(
    validator: &mut Validator,
    context: &ValidationContext,
    access_index: &AstStatement,
    access_type: &DirectAccessType,
    target_type: &DataTypeInformation,
    location: &SourceRange,
) {
    match *access_index {
        AstStatement::LiteralInteger { value, .. } => {
            if !access_type.is_in_range(value.try_into().unwrap_or_default(), target_type, context.index) {
                validator.push_diagnostic(Diagnostic::incompatible_directaccess_range(
                    &format!("{access_type:?}"),
                    target_type.get_name(),
                    access_type.get_range(target_type, context.index),
                    location.clone(),
                ))
            }
        }
        AstStatement::Reference { .. } => {
            let ref_type = context.annotations.get_type_or_void(access_index, context.index);
            if !ref_type.get_type_information().is_int() {
                validator.push_diagnostic(Diagnostic::incompatible_directaccess_variable(
                    ref_type.get_name(),
                    location.clone(),
                ))
            }
        }
        _ => unreachable!(),
    }
}

fn validate_reference(
    validator: &mut Validator,
    statement: &AstStatement,
    ref_name: &str,
    location: &SourceRange,
    context: &ValidationContext,
) {
    // unresolved reference
    if !context.annotations.has_type_annotation(statement) {
        validator.push_diagnostic(Diagnostic::unresolved_reference(ref_name, location.clone()));
    } else if let Some(StatementAnnotation::Variable { qualified_name, variable_type, .. }) =
        context.annotations.get(statement)
    {
        // check if we're accessing a private variable AND the variable's qualifier is not the
        // POU we're accessing it from
        if variable_type.is_private()
            && context
                .qualifier
                .and_then(|qualifier| context.index.find_pou(qualifier))
                .map(|pou| (pou.get_name(), pou.get_container())) // get the container pou (for actions this is the program/fb)
                .map_or(false, |(pou, container)| {
                    !qualified_name.starts_with(pou) && !qualified_name.starts_with(container)
                })
        {
            validator.push_diagnostic(Diagnostic::illegal_access(qualified_name.as_str(), location.clone()));
        }
    }
}

fn visit_array_access(
    validator: &mut Validator,
    reference: &AstStatement,
    access: &AstStatement,
    context: &ValidationContext,
) {
    let target_type = context.annotations.get_type_or_void(reference, context.index).get_type_information();

    if let DataTypeInformation::Array { dimensions, .. } = target_type {
        if let AstStatement::ExpressionList { expressions, .. } = access {
            for (i, exp) in expressions.iter().enumerate() {
                validate_array_access(validator, exp, dimensions, i, context);
            }
        } else {
            validate_array_access(validator, access, dimensions, 0, context);
        }
    } else {
        validator.push_diagnostic(Diagnostic::incompatible_array_access_variable(
            target_type.get_name(),
            access.get_location(),
        ));
    }
}

fn validate_array_access(
    validator: &mut Validator,
    access: &AstStatement,
    dimensions: &[Dimension],
    dimension_index: usize,
    context: &ValidationContext,
) {
    if let AstStatement::LiteralInteger { value, .. } = access {
        if let Some(dimension) = dimensions.get(dimension_index) {
            if let Ok(range) = dimension.get_range(context.index) {
                if !(range.start as i128 <= *value && range.end as i128 >= *value) {
                    validator.push_diagnostic(Diagnostic::incompatible_array_access_range(
                        range,
                        access.get_location(),
                    ))
                }
            }
        }
    } else {
        let type_info = context.annotations.get_type_or_void(access, context.index).get_type_information();
        if !type_info.is_int() {
            validator.push_diagnostic(Diagnostic::incompatible_array_access_type(
                type_info.get_name(),
                access.get_location(),
            ))
        }
    }
}

fn visit_binary_expression(
    validator: &mut Validator,
    statement: &AstStatement,
    operator: &Operator,
    left: &AstStatement,
    right: &AstStatement,
    context: &ValidationContext,
) {
    match operator {
        Operator::NotEqual => {
            validate_binary_expression(validator, statement, &Operator::Equal, left, right, context)
        }
        Operator::GreaterOrEqual => {
            // check for the > operator
            validate_binary_expression(validator, statement, &Operator::Greater, left, right, context);
            // check for the = operator
            validate_binary_expression(validator, statement, &Operator::Equal, left, right, context);
        }
        Operator::LessOrEqual => {
            // check for the < operator
            validate_binary_expression(validator, statement, &Operator::Less, left, right, context);
            // check for the = operator
            validate_binary_expression(validator, statement, &Operator::Equal, left, right, context);
        }
        _ => validate_binary_expression(validator, statement, operator, left, right, context),
    }
}

fn validate_binary_expression(
    validator: &mut Validator,
    statement: &AstStatement,
    operator: &Operator,
    left: &AstStatement,
    right: &AstStatement,
    context: &ValidationContext,
) {
    let left_type = context.annotations.get_type_or_void(left, context.index).get_type_information();
    let right_type = context.annotations.get_type_or_void(right, context.index).get_type_information();

    // if the type is a subrange, check if the intrinsic type is numerical
    let is_numerical = context.index.find_intrinsic_type(left_type).is_numerical();

    if std::mem::discriminant(left_type) == std::mem::discriminant(right_type)
        && !(is_numerical || left_type.is_pointer())
    {
        // see if we have the right compare-function (non-numbers are compared using user-defined callback-functions)
        if operator.is_comparison_operator()
            && !compare_function_exists(left_type.get_name(), operator, context)
        {
            validator.push_diagnostic(Diagnostic::missing_compare_function(
                crate::typesystem::get_equals_function_name_for(left_type.get_name(), operator)
                    .unwrap_or_default()
                    .as_str(),
                left_type.get_name(),
                statement.get_location(),
            ));
        }
    }
}

fn compare_function_exists(type_name: &str, operator: &Operator, context: &ValidationContext) -> bool {
    let implementation = get_equals_function_name_for(type_name, operator)
        .as_ref()
        .and_then(|function_name| context.index.find_pou_implementation(function_name));

    if let Some(implementation) = implementation {
        let members = context.index.get_pou_members(implementation.get_type_name());

        // we expect two input parameters and a return-parameter
        if let [VariableIndexEntry {
            data_type_name: type_name_1,
            variable_type: ArgumentType::ByVal(VariableType::Input),
            ..
        }, VariableIndexEntry {
            data_type_name: type_name_2,
            variable_type: ArgumentType::ByVal(VariableType::Input),
            ..
        }, VariableIndexEntry {
            data_type_name: return_type,
            variable_type: ArgumentType::ByVal(VariableType::Return),
            ..
        }] = members
        {
            let type_name_1 = context
                .index
                .get_effective_type_or_void_by_name(type_name_1)
                .get_type_information()
                .get_name();
            let type_name_2 = context
                .index
                .get_effective_type_or_void_by_name(type_name_2)
                .get_type_information()
                .get_name();

            // both parameters must have the same type and the return type must be BOOL
            if type_name_1 == type_name && type_name_2 == type_name && return_type == BOOL_TYPE {
                return true;
            }
        }
    }

    false
}

fn validate_unary_expression(
    validator: &mut Validator,
    operator: &Operator,
    value: &AstStatement,
    location: &SourceRange,
) {
    if operator == &Operator::Address {
        match value {
            AstStatement::Reference { .. }
            | AstStatement::QualifiedReference { .. }
            | AstStatement::ArrayAccess { .. } => (),

            _ => validator.push_diagnostic(Diagnostic::invalid_operation(
                "Invalid address-of operation",
                location.to_owned(),
            )),
        }
    }
}

/// Validates if an argument can be passed to a function with [`VariableType::Output`] and
/// [`VariableType::InOut`] parameter types by checking if the argument is a reference (e.g. `foo(x)`) or
/// an assignment (e.g. `foo(x := y)`, `foo(x => y)`). If neither is the case a diagnostic is generated.
fn validate_call_by_ref(validator: &mut Validator, param: &VariableIndexEntry, arg: &AstStatement) {
    let ty = param.variable_type.get_variable_type();
    if !matches!(ty, VariableType::Output | VariableType::InOut) {
        return;
    }

    match (arg.can_be_assigned_to(), arg) {
        (true, _) => (),

        // Output assignments are optional, e.g. `foo(bar => )` is considered valid
        (false, AstStatement::EmptyStatement { .. }) if matches!(ty, VariableType::Output) => (),

        (false, AstStatement::Assignment { right, .. } | AstStatement::OutputAssignment { right, .. }) => {
            validate_call_by_ref(validator, param, right);
        }

        _ => validator.push_diagnostic(Diagnostic::invalid_argument_type(
            param.get_name(),
            param.get_variable_type(),
            arg.get_location(),
        )),
    }
}

fn validate_assignment(
    validator: &mut Validator,
    right: &AstStatement,
    left: Option<&AstStatement>,
    location: &SourceRange,
    context: &ValidationContext,
) {
    if let Some(left) = left {
        // check if we assign to a constant variable
        if let Some(StatementAnnotation::Variable { constant, qualified_name, .. }) =
            context.annotations.get(left)
        {
            if *constant {
                validator.push_diagnostic(Diagnostic::cannot_assign_to_constant(
                    qualified_name.as_str(),
                    left.get_location(),
                ));
            }
        }

        // If whatever we got is not assignable, output an error
        if !left.can_be_assigned_to() {
            // we hit an assignment without a LValue to assign to
            validator.push_diagnostic(Diagnostic::reference_expected(left.get_location()));
        }
    }

    let right_type = context.annotations.get_type(right, context.index);
    let left_type = context.annotations.get_type_hint(right, context.index);
    if let (Some(right_type), Some(left_type)) = (right_type, left_type) {
        // implicit call parameter assignments are annotated to auto_deref pointers for ´ByRef` parameters
        // we need the inner type
        let left_type = if let DataTypeInformation::Pointer { inner_type_name, auto_deref: true, .. } =
            left_type.get_type_information()
        {
            context.index.get_effective_type_or_void_by_name(inner_type_name)
        } else {
            left_type
        };

        if !left_type.is_compatible_with_type(right_type)
            || !is_valid_assignment(left_type, right_type, right, context.index, location, validator)
        {
            validator.push_diagnostic(Diagnostic::invalid_assignment(
                right_type.get_type_information().get_name(),
                left_type.get_type_information().get_name(),
                location.clone(),
            ));
        } else if !right.is_literal() {
            validate_assignment_type_sizes(validator, left_type, right_type, location, context)
        }
    }
}

fn is_valid_assignment(
    left_type: &DataType,
    right_type: &DataType,
    right: &AstStatement,
    index: &Index,
    location: &SourceRange,
    validator: &mut Validator,
) -> bool {
    if is_valid_string_to_char_assignment(
        left_type.get_type_information(),
        right_type.get_type_information(),
        right,
        location,
        validator,
    ) {
        // in this case return true and skip any other validation
        // because those would fail
        return true;
    } else if is_invalid_pointer_assignment(
        left_type.get_type_information(),
        right_type.get_type_information(),
        index,
        location,
        validator,
    ) | is_invalid_char_assignment(
        left_type.get_type_information(),
        right_type.get_type_information(),
    ) | is_aggregate_to_none_aggregate_assignment(left_type, right_type)
        | is_aggregate_type_missmatch(left_type, right_type, index)
    {
        return false;
    }
    true
}

/// strings with length 1 can be assigned to characters
fn is_valid_string_to_char_assignment(
    left_type: &DataTypeInformation,
    right_type: &DataTypeInformation,
    right: &AstStatement,
    location: &SourceRange,
    validator: &mut Validator,
) -> bool {
    // TODO: casted literals and reference
    if left_type.is_compatible_char_and_string(right_type) {
        if let AstStatement::LiteralString { value, .. } = right {
            if value.len() == 1 {
                return true;
            } else {
                validator.push_diagnostic(Diagnostic::syntax_error(
                    format!("Value: '{value}' exceeds length for type: {}", left_type.get_name()).as_str(),
                    location.clone(),
                ));
                return false;
            }
        }
    }
    false
}

fn is_invalid_pointer_assignment(
    left_type: &DataTypeInformation,
    right_type: &DataTypeInformation,
    index: &Index,
    location: &SourceRange,
    validator: &mut Validator,
) -> bool {
    if left_type.is_pointer() & right_type.is_pointer() {
        return !typesystem::is_same_type_class(left_type, right_type, index);
    }
    //check if Datatype can hold a Pointer (u64)
    else if right_type.is_pointer()
        && !left_type.is_pointer()
        && left_type.get_size_in_bits(index) < POINTER_SIZE
    {
        validator.push_diagnostic(Diagnostic::incompatible_type_size(
            left_type.get_name(),
            left_type.get_size_in_bits(index),
            "hold a",
            location.clone(),
        ));
        return true;
    }
    //check if size allocated to Pointer is standart pointer size (u64)
    else if left_type.is_pointer()
        && !right_type.is_pointer()
        && right_type.get_size_in_bits(index) < POINTER_SIZE
    {
        validator.push_diagnostic(Diagnostic::incompatible_type_size(
            right_type.get_name(),
            right_type.get_size_in_bits(index),
            "to be stored in a",
            location.clone(),
        ));
        return true;
    }
    false
}

/// check if we try to assign a CHAR to WCHAR or vice versa
fn is_invalid_char_assignment(left_type: &DataTypeInformation, right_type: &DataTypeInformation) -> bool {
    if (left_type.is_character() & right_type.is_character())
        && (left_type.get_name() != right_type.get_name())
    {
        return true;
    }
    false
}

/// aggregate types can only be assigned to aggregate types
/// special case char := string_with_length_1, handled by `is_valid_string_to_char_assignment()`
fn is_aggregate_to_none_aggregate_assignment(left_type: &DataType, right_type: &DataType) -> bool {
    left_type.is_aggregate_type() ^ right_type.is_aggregate_type()
}

/// if we try to assign an aggregate type to another
/// check if we have the same type
fn is_aggregate_type_missmatch(left_type: &DataType, right_type: &DataType, index: &Index) -> bool {
    left_type.is_aggregate_type() & right_type.is_aggregate_type()
        && !typesystem::is_same_type_class(
            left_type.get_type_information(),
            right_type.get_type_information(),
            index,
        )
}

fn validate_call(
    validator: &mut Validator,
    operator: &AstStatement,
    parameters: &Option<AstStatement>,
    context: &ValidationContext,
) {
    // visit called pou
    visit_statement(validator, operator, context);

    if let Some(pou) = context.find_pou(operator) {
        let declared_parameters = context.index.get_declared_parameters(pou.get_name());
        let passed_parameters = parameters.as_ref().map(ast::flatten_expression_list).unwrap_or_default();

        let mut passed_params_idx = Vec::new();
        let mut are_implicit_parameters = true;
        // validate parameters
        for (i, p) in passed_parameters.iter().enumerate() {
            if let Ok((location_in_parent, right, is_implicit)) =
                get_implicit_call_parameter(p, &declared_parameters, i)
            {
                // safe index of passed parameter
                passed_params_idx.push(location_in_parent);

                let left = declared_parameters.get(location_in_parent);
                if let Some(left) = left {
                    validate_call_by_ref(validator, left, p);
                }

                // explicit call parameter assignments will be handled by
                // `visit_statement()` via `Assignment` and `OutputAssignment`
                if is_implicit {
                    validate_assignment(validator, right, None, &p.get_location(), context);
                }

                // mixing implicit and explicit parameters is not allowed
                // allways compare to the first passed parameter
                if i == 0 {
                    are_implicit_parameters = is_implicit;
                } else if are_implicit_parameters != is_implicit {
                    validator.push_diagnostic(Diagnostic::invalid_parameter_type(p.get_location()));
                }
            }

            visit_statement(validator, p, context);
        }

        // for PROGRAM/FB we need special inout validation
        if let PouIndexEntry::FunctionBlock { .. } | PouIndexEntry::Program { .. } = pou {
            let inouts: Vec<&&VariableIndexEntry> =
                declared_parameters.iter().filter(|p| VariableType::InOut == p.get_variable_type()).collect();
            // if the called pou has declared inouts, we need to make sure that these were passed to the pou call
            if !inouts.is_empty() {
                // check if all inouts were passed to the pou call
                inouts.into_iter().for_each(|p| {
                    if !passed_params_idx.contains(&(p.get_location_in_parent() as usize)) {
                        validator.push_diagnostic(Diagnostic::missing_inout_parameter(
                            p.get_name(),
                            operator.get_location(),
                        ));
                    }
                });
            }
        }
    } else {
        // POU could not be found, we can still partially validate the passed parameters
        if let Some(s) = parameters.as_ref() {
            visit_statement(validator, s, context);
        }
    }
}

// selector, case_blocks, else_block
fn validate_case_statement(
    validator: &mut Validator,
    selector: &AstStatement,
    case_blocks: &[ConditionalBlock],
    else_block: &[AstStatement],
    context: &ValidationContext,
) {
    visit_statement(validator, selector, context);

    let mut cases = HashSet::new();
    case_blocks.iter().for_each(|b| {
        let condition = b.condition.as_ref();

        // invalid case conditions
        if matches!(condition, AstStatement::Assignment { .. } | AstStatement::CallStatement { .. }) {
            validator.push_diagnostic(Diagnostic::invalid_case_condition(condition.get_location()));
        }

        // validate for duplicate conditions
        // first try to evaluate the conditions value
        const_evaluator::evaluate(condition, context.qualifier, context.index)
            .map_err(|err| {
                // value evaluation and validation not possible with non constants
                validator
                    .push_diagnostic(Diagnostic::non_constant_case_condition(&err, condition.get_location()))
            })
            .map(|v| {
                // check for duplicates if we got a value
                if let Some(AstStatement::LiteralInteger { value, .. }) = v {
                    if !cases.insert(value) {
                        validator.push_diagnostic(Diagnostic::duplicate_case_condition(
                            &value,
                            condition.get_location(),
                        ));
                    }
                };
            })
            .ok(); // no need to worry about the result

        visit_statement(validator, condition, context);
        b.body.iter().for_each(|s| visit_statement(validator, s, context));
    });

    else_block.iter().for_each(|s| visit_statement(validator, s, context));
}

/// Validates that the assigned type and type hint are compatible with the nature for this
/// statement
fn validate_type_nature(validator: &mut Validator, statement: &AstStatement, context: &ValidationContext) {
    if let Some(type_hint) = context
        .annotations
        .get_type_hint(statement, context.index)
        .or_else(|| context.annotations.get_type(statement, context.index))
    {
        if let DataTypeInformation::Generic { generic_symbol, nature, .. } = type_hint.get_type_information()
        {
            validator.push_diagnostic(Diagnostic::unresolved_generic_type(
                generic_symbol,
                &format!("{nature:?}"),
                statement.get_location(),
            ))
        } else if let Some((actual_type, generic_nature)) = context
            .annotations
            .get_type(statement, context.index)
            .zip(context.annotations.get_generic_nature(statement))
        {
            // check if type_hint and actual_type is compatible
            // should be handled by assignment validation
            if !(actual_type.has_nature(*generic_nature, context.index)
				// INT parameter for REAL is allowed
                | (type_hint.is_real() & actual_type.is_numerical()))
            {
                validator.push_diagnostic(Diagnostic::invalid_type_nature(
                    actual_type.get_name(),
                    format!("{generic_nature:?}").as_str(),
                    statement.get_location(),
                ));
            }
        }
    }
}

fn validate_assignment_type_sizes(
    validator: &mut Validator,
    left: &DataType,
    right: &DataType,
    location: &SourceRange,
    context: &ValidationContext,
) {
    if left.get_type_information().get_size(context.index)
        < right.get_type_information().get_size(context.index)
    {
        validator.push_diagnostic(Diagnostic::implicit_downcast(
            left.get_name(),
            right.get_name(),
            location.clone(),
        ))
    }
}