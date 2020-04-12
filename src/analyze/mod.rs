#![allow(unused_variables)]

mod expr;
mod init;
mod stmt;

use std::collections::{HashSet, VecDeque};

use counter::Counter;

use crate::data::{error::Warning, hir::*, lex::Keyword, *};
use crate::intern::InternedStr;
use crate::parse::{Lexer, Parser};

pub(crate) type TagScope = Scope<InternedStr, TagEntry>;

#[derive(Clone, Debug)]
pub(crate) enum TagEntry {
    Struct(StructRef),
    Union(StructRef),
    // list of (name, value)s
    Enum(Vec<(InternedStr, i64)>),
}

pub struct Analyzer<T: Lexer> {
    declarations: Parser<T>,
    // in case a `Declaration` has multiple declarators
    pending: VecDeque<Locatable<Declaration>>,
    /// objects that are in scope
    /// C actually has 4 different scopes:
    /// 1. ordinary identifiers
    /// 2. tags
    /// 3. label names
    /// 4. members
    ///
    /// This holds the scope for ordinary identifiers: variables and typedefs
    scope: Scope<InternedStr, MetadataRef>,
    /// the compound types that have been declared (struct/union/enum)
    /// scope 2. from above
    tag_scope: TagScope,
    /// Stores all variables that have been initialized so far
    initialized: HashSet<MetadataRef>,
    /// Internal API which makes it easier to return errors lazily
    error_handler: ErrorHandler,
}

impl<T: Lexer> Iterator for Analyzer<T> {
    type Item = CompileResult<Locatable<Declaration>>;
    fn next(&mut self) -> Option<Self::Item> {
        // have to handle `int;` somehow
        loop {
            // Instead of returning `SemanticResult`, the analyzer puts all errors into `error_handler`.
            // This simplifies the logic in `next` greatly.
            // NOTE: this returns errors for a declaration before the declaration itself
            if let Some(err) = self.error_handler.pop_front() {
                return Some(Err(err));
            // If we saw `int i, j, k;`, we treated those as different declarations
            // `j, k` will be stored into `pending`
            } else if let Some(decl) = self.pending.pop_front() {
                return Some(Ok(decl));
            }
            // Now do the real work.
            let next = match self.declarations.next()? {
                Err(err) => return Some(Err(err)),
                Ok(decl) => decl,
            };
            // Note that we need to store `next` so that we have the location in case it was empty.
            let location = next.location;
            let decls = self.parse_external_declaration(next);
            if decls.is_empty() {
                self.warn(Warning::EmptyDeclaration, location);
            } else {
                // TODO: if an error occurs, should we still add the declaration to `pending`?
                self.pending.extend(decls);
            }
        }
    }
}

impl<I: Lexer> Analyzer<I> {
    pub fn new(parser: Parser<I>) -> Self {
        Self {
            declarations: parser,
            error_handler: ErrorHandler::new(),
            scope: Scope::new(),
            tag_scope: Scope::new(),
            pending: VecDeque::new(),
            initialized: HashSet::new(),
        }
    }
    /// Return all warnings seen so far.
    ///
    /// These warnings are consumed and will not be returned if you call
    /// `warnings()` again.
    pub fn warnings(&mut self) -> VecDeque<CompileWarning> {
        std::mem::replace(&mut self.error_handler.warnings, Default::default())
    }
    // I type these a lot
    #[inline(always)]
    fn err(&mut self, e: SemanticError, l: Location) {
        self.error_handler.error(e, l);
    }
    #[inline(always)]
    fn warn(&mut self, w: Warning, l: Location) {
        self.error_handler.warn(w, l);
    }
    fn parse_external_declaration(
        &mut self, next: Locatable<ast::ExternalDeclaration>,
    ) -> Vec<Locatable<Declaration>> {
        use ast::ExternalDeclaration;

        match next.data {
            ExternalDeclaration::Function(func) => {
                let id = func.id;
                let (meta_ref, body) = FunctionAnalyzer::analyze(func, self, next.location);
                self.scope.insert(id, meta_ref);
                let decl = Declaration {
                    symbol: meta_ref,
                    init: Some(Initializer::FunctionBody(body)),
                };
                vec![Locatable::new(decl, next.location)]
            }
            ExternalDeclaration::Declaration(declaration) => {
                self.parse_declaration(declaration, next.location)
            }
        }
    }
    fn parse_declaration(
        &mut self, declaration: ast::Declaration, location: Location,
    ) -> Vec<Locatable<Declaration>> {
        let original = self.parse_specifiers(declaration.specifiers, location);

        if original.storage_class == Some(StorageClass::Auto) && self.scope.is_global() {
            self.err(SemanticError::AutoAtGlobalScope, location);
        }

        // TODO: this is such a hack: https://github.com/jyn514/rcc/issues/371
        let sc = original.storage_class.unwrap_or(StorageClass::Auto);
        let mut decls = Vec::new();
        for d in declaration.declarators {
            let ctype =
                self.parse_declarator(original.ctype.clone(), d.data.declarator.decl, d.location);

            if !ctype.is_function() && original.qualifiers.func != FunctionQualifiers::default() {
                self.err(
                    SemanticError::FuncQualifiersNotAllowed(original.qualifiers.func),
                    d.location,
                );
            }

            let id = d.data.declarator.id;
            let id = id.expect("declarations should never be abstract");
            let init = if let Some(init) = d.data.init {
                Some(self.parse_initializer(init, &ctype, d.location))
            } else {
                None
            };
            if sc == StorageClass::Typedef {
                self.declarations.typedefs.insert(id, ());
            }
            let symbol = Metadata {
                ctype,
                id,
                qualifiers: original.qualifiers,
                storage_class: sc,
            }
            .insert();
            if init.is_some() {
                self.initialized.insert(symbol);
            }
            self.declare(symbol, d.location);
            decls.push(Locatable::new(Declaration { symbol, init }, d.location));
        }
        decls
    }
    #[cfg(test)]
    #[inline(always)]
    pub(crate) fn parse_typename_test(&mut self, ctype: ast::TypeName, location: Location) -> Type {
        self.parse_typename(ctype, location)
    }
    // TODO: I don't think this is a very good abstraction
    fn parse_typename(&mut self, ctype: ast::TypeName, location: Location) -> Type {
        let parsed = self.parse_type(ctype.specifiers, ctype.declarator.decl, location);
        // TODO: should these be syntax errors instead?
        if let Some(sc) = parsed.storage_class {
            self.err(SemanticError::IllegalStorageClass(sc), location);
        }
        if parsed.qualifiers != Qualifiers::default() {
            self.warn(Warning::IgnoredQualifier(parsed.qualifiers), location);
        }
        if let Some(id) = ctype.declarator.id {
            self.err(SemanticError::IdInTypeName(id), location);
        }
        parsed.ctype
    }
    fn parse_type(
        &mut self, specifiers: Vec<ast::DeclarationSpecifier>, declarator: ast::DeclaratorType,
        location: Location,
    ) -> ParsedType {
        let mut specs = self.parse_specifiers(specifiers, location);
        specs.ctype = self.parse_declarator(specs.ctype, declarator, location);

        if !specs.ctype.is_function() && specs.qualifiers.func != FunctionQualifiers::default() {
            self.err(
                SemanticError::FuncQualifiersNotAllowed(specs.qualifiers.func),
                location,
            );
        }

        specs
    }
    fn parse_specifiers(
        &mut self, specifiers: Vec<ast::DeclarationSpecifier>, location: Location,
    ) -> ParsedType {
        use ast::{DeclarationSpecifier::*, UnitSpecifier::*};

        // need to parse specifiers now
        // it's not enough to collect into a `Set` since `long long` has a different meaning than `long`
        // instead, we see how many times each specifier is present
        // however, for some specifiers this doesn't really make sense:
        // if we see `struct s { int i; }` twice in a row,
        // it's more likely that the user forgot a semicolon in between than try to make some weird double struct type.
        // so: count the specifiers that are keywords and store the rest somewhere out of the way

        // TODO: initialization is a mess
        let (counter, compounds) = count_specifiers(specifiers, &mut self.error_handler, location);
        // Now that we've separated this into unit specifiers and compound specifiers,
        // see if we can pick up the proper types and qualifiers.
        let signed = match (counter.get(&Signed), counter.get(&Unsigned)) {
            (None, None) | (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some(_), Some(_)) => {
                self.err(SemanticError::ConflictingSigned, location);
                true
            }
        };
        // `long` is special because of `long long` and `long double`
        let mut ctype = None;
        if let Some(&long_count) = counter.get(&Long) {
            match long_count {
                0 => panic!("constraint violation, should only set count if > 0"),
                1 => {
                    // NOTE: this is handled later by the big `for type in [...]` loop
                    // see notes there
                    if counter.get(&Double).is_none() {
                        ctype = Some(Type::Long(signed));
                    }
                }
                // TODO: implement `long long` as a separate type
                2 => ctype = Some(Type::Long(signed)),
                _ => {
                    self.err(SemanticError::TooLong(long_count), location);
                    ctype = Some(Type::Long(signed));
                }
            }
        }
        let qualifiers = Qualifiers {
            c_const: counter.get(&Const).is_some(),
            volatile: counter.get(&Volatile).is_some(),
            func: FunctionQualifiers {
                inline: counter.get(&Inline).is_some(),
                no_return: counter.get(&NoReturn).is_some(),
            },
        };
        let mut storage_class = None;
        for (spec, sc) in &[
            (Auto, StorageClass::Auto),
            (Register, StorageClass::Register),
            (Static, StorageClass::Static),
            (Extern, StorageClass::Extern),
            (UnitSpecifier::Typedef, StorageClass::Typedef),
        ] {
            if counter.get(spec).is_some() {
                if let Some(existing) = storage_class {
                    self.err(
                        SemanticError::ConflictingStorageClass(existing, *sc),
                        location,
                    );
                }
                storage_class = Some(*sc);
            }
        }
        // TODO: maybe use `iter!` macro instead of `vec!` to avoid an allocation?
        // https://play.rust-lang.org/?gist=0535aa4f749a14cb1b28d658446f3c13
        for (spec, new_ctype) in vec![
            (Bool, Type::Bool),
            (Char, Type::Char(signed)),
            (Short, Type::Short(signed)),
            (Int, Type::Int(signed)),
            // already handled `long` when we handled `long long`
            (Float, Type::Float),
            // NOTE: if we saw `long double` before, we'll set `ctype` to `double` now
            // TODO: make `long double` different from `double`
            (Double, Type::Double),
            (Void, Type::Void),
            (VaList, Type::VaList),
        ] {
            if counter.get(&spec).is_some() {
                if let Some(existing) = ctype {
                    self.err(
                        SemanticError::ConflictingType(existing, new_ctype.clone()),
                        location,
                    );
                }
                ctype = Some(new_ctype);
            }
        }
        let mut declared_compound_type = false;
        for compound in compounds {
            let parsed = match compound {
                Unit(_) => unreachable!("already caught"),
                DeclarationSpecifier::Typedef(name) => {
                    let meta = self
                        .scope
                        .get(&name)
                        .expect("scope of parser and analyzer should match")
                        .get();
                    assert_eq!(meta.storage_class, StorageClass::Typedef);
                    meta.ctype.clone()
                }
                Struct(s) => self.struct_specifier(s, true, &mut declared_compound_type, location),
                Union(s) => self.struct_specifier(s, false, &mut declared_compound_type, location),
                Enum { name, members } => {
                    self.enum_specifier(name, members, &mut declared_compound_type, location)
                }
            };
            // TODO: this should report the name of the typedef, not the type itself
            if let Some(existing) = &ctype {
                self.err(
                    SemanticError::ConflictingType(existing.clone(), parsed.clone()),
                    location,
                );
            }
            ctype = Some(parsed);
        }
        // Check to see if we had a conflicting `signed` specifier
        // Note we use `counter` instead of the `signed` bool
        // because we've already set the default and forgotten whether it was originally present.
        if counter.get(&Signed).is_some() || counter.get(&Unsigned).is_some() {
            match &ctype {
                // unsigned int
                Some(Type::Char(_)) | Some(Type::Short(_)) | Some(Type::Int(_))
                | Some(Type::Long(_)) => {}
                // unsigned float
                Some(other) => {
                    let err = SemanticError::CannotBeSigned(other.clone());
                    self.err(err, location);
                }
                // unsigned i
                None => ctype = Some(Type::Int(signed)),
            }
        }
        // i;
        let ctype = ctype.unwrap_or_else(|| {
            self.warn(Warning::ImplicitInt, location);
            Type::Int(true)
        });
        ParsedType {
            qualifiers,
            storage_class,
            ctype,
            declared_compound_type,
        }
    }
    fn struct_specifier(
        &mut self, struct_spec: ast::StructSpecifier, is_struct: bool, declared_struct: &mut bool,
        location: Location,
    ) -> Type {
        let ast_members = match struct_spec.members {
            Some(members) => members,
            None => {
                let name = if let Some(name) = struct_spec.name {
                    name
                } else {
                    let err = SemanticError::from("bare 'enum' as type specifier is not allowed");
                    self.error_handler.error(err, location);
                    return Type::Error;
                };
                match (is_struct, self.tag_scope.get(&name)) {
                    (true, Some(TagEntry::Struct(s))) => {
                        return Type::Struct(StructType::Named(name, *s));
                    }
                    (false, Some(TagEntry::Union(s))) => {
                        return Type::Union(StructType::Named(name, *s));
                    }
                    (_, Some(other)) => {
                        let kind = if is_struct { "struct" } else { "union " };
                        let err = SemanticError::from(format!("use of '{}' with type tag '{}' that does not match previous struct declaration", name, kind));
                        self.error_handler.push_back(Locatable::new(err, location));
                        return Type::Error;
                    }
                    (_, None) => {
                        return self.forward_declaration(
                            if is_struct {
                                Keyword::Struct
                            } else {
                                Keyword::Union
                            },
                            name,
                            location,
                        )
                    }
                }
            }
        };
        let members: Vec<_> = ast_members
            .into_iter()
            .map(|m| self.struct_declarator_list(m, location).into_iter())
            .flatten()
            .collect();
        if members.is_empty() {
            self.err(SemanticError::from("cannot have empty struct"), location);
            return Type::Error;
        }
        let constructor = if is_struct { Type::Struct } else { Type::Union };
        if let Some(id) = struct_spec.name {
            let struct_ref = if let Some(TagEntry::Struct(struct_ref))
            | Some(TagEntry::Union(struct_ref)) =
                self.tag_scope.get_immediate(&id)
            {
                let struct_ref = *struct_ref;
                if !struct_ref.get().is_empty() {
                    self.err(
                        SemanticError::from(format!(
                            "redefinition of {} '{}'",
                            if is_struct { "struct" } else { "union" },
                            id
                        )),
                        location,
                    );
                }
                struct_ref
            } else {
                StructRef::new()
            };
            struct_ref.update(members);
            let entry = if is_struct {
                TagEntry::Struct
            } else {
                TagEntry::Union
            }(struct_ref);
            self.tag_scope.insert(id, entry);
            *declared_struct = true;
            constructor(StructType::Named(id, struct_ref))
        } else {
            constructor(StructType::Anonymous(std::rc::Rc::new(members)))
        }
    }
    /*
    struct_declarator_list: struct_declarator (',' struct_declarator)* ;
    struct_declarator
        : declarator
        | ':' constant_expr  // bitfield, not supported
        | declarator ':' constant_expr
        ;
    */
    fn struct_declarator_list(
        &mut self, members: ast::StructDeclarationList, location: Location,
    ) -> Vec<Metadata> {
        let parsed_type = self.parse_specifiers(members.specifiers, location);
        if parsed_type.qualifiers.has_func_qualifiers() {
            self.err(
                SemanticError::FuncQualifiersNotAllowed(parsed_type.qualifiers.func),
                location,
            );
        }

        let mut parsed_members = Vec::new();
        for ast::StructDeclarator { decl, bitfield } in members.declarators {
            let decl = match decl {
                None => continue,
                Some(d) => d,
            };
            let ctype = self.parse_declarator(parsed_type.ctype.clone(), decl.decl, location);
            let mut symbol = Metadata {
                storage_class: StorageClass::Auto,
                qualifiers: parsed_type.qualifiers,
                ctype,
                id: decl.id.expect("struct members should have an id"),
            };
            if let Some(bitfield) = bitfield {
                let bit_size = match Self::const_int(self.parse_expr(bitfield), false) {
                    Ok(e) => e,
                    Err(err) => {
                        self.error_handler.push_back(err);
                        1
                    }
                };
                let type_size = symbol.ctype.sizeof().unwrap_or(0);
                if bit_size == 0 {
                    let err = SemanticError::from(format!(
                        "C does not have zero-sized types. hint: omit the declarator {}",
                        symbol.id
                    ));
                    self.err(err, location);
                } else if bit_size > type_size * u64::from(crate::arch::CHAR_BIT) {
                    let err = SemanticError::from(format!(
                        "cannot have bitfield {} with size {} larger than containing type {}",
                        symbol.id, bit_size, symbol.ctype
                    ));
                    self.err(err, location);
                }
                self.error_handler.warn(
                    "bitfields are not implemented and will be ignored",
                    location,
                );
            }
            match symbol.ctype {
                Type::Struct(StructType::Named(_, inner_members))
                | Type::Union(StructType::Named(_, inner_members))
                    if inner_members.get().is_empty() =>
                {
                    self.err(
                        SemanticError::from(format!(
                            "cannot use type '{}' before it has been defined",
                            symbol.ctype
                        )),
                        location,
                    );
                    // add this as a member anyway because
                    // later code depends on structs being non-empty
                    symbol.ctype = Type::Error;
                }
                _ => {}
            }
            parsed_members.push(symbol);
        }
        if let Some(class) = parsed_type.storage_class {
            let member = parsed_members
                .last()
                .expect("should have seen at least one declaration");
            self.err(
                SemanticError::from(format!(
                    "cannot specify storage class '{}' for struct member '{}'",
                    class, member.id,
                )),
                location,
            );
        }
        parsed_members
    }
    fn enum_specifier(
        &mut self, enum_name: Option<InternedStr>,
        ast_members: Option<Vec<(InternedStr, Option<ast::Expr>)>>, saw_enum: &mut bool,
        location: Location,
    ) -> Type {
        let ast_members = match ast_members {
            Some(members) => members,
            None => {
                let name = if let Some(name) = enum_name {
                    name
                } else {
                    let err = SemanticError::from("bare 'enum' as type specifier is not allowed");
                    self.error_handler.error(err, location);
                    return Type::Error;
                };
                match self.tag_scope.get(&name) {
                    Some(TagEntry::Enum(members)) => {
                        return Type::Enum(Some(name), members.clone());
                    }
                    Some(other) => {
                        let err = SemanticError::from(format!("use of '{}' with type tag 'enum' that does not match previous struct declaration", name));
                        self.error_handler.push_back(Locatable::new(err, location));
                        return Type::Error;
                    }
                    None => return self.forward_declaration(Keyword::Enum, name, location),
                }
            }
        };

        let mut discriminant = 0;
        let mut members = vec![];
        for (name, maybe_value) in ast_members {
            if let Some(value) = maybe_value {
                let location = value.location;
                discriminant = Self::const_int(self.parse_expr(value), true).map_or_else(
                    |err| {
                        self.error_handler.push_back(err);
                        std::i64::MIN
                    },
                    |unsigned| unsigned as i64,
                );
            }
            members.push((name, discriminant));
            // TODO: this is such a hack
            let tmp_symbol = Metadata {
                id: name,
                qualifiers: Qualifiers {
                    c_const: true,
                    ..Default::default()
                },
                storage_class: StorageClass::Register,
                ctype: Type::Enum(None, vec![(name, discriminant)]),
            };
            self.scope.insert(name, tmp_symbol.insert());
            discriminant = discriminant.checked_add(1).unwrap_or_else(|| {
                self.error_handler
                    .push_back(location.error(SemanticError::EnumOverflow));
                0
            });
        }
        for (name, _) in &members {
            self.scope._remove(name);
        }
        if members.is_empty() {
            self.err(SemanticError::from("enums cannot be empty"), location)
        }
        if let Some(id) = enum_name {
            if self
                .tag_scope
                .insert(id.clone(), TagEntry::Enum(members.clone()))
                .is_some()
            {
                self.err(format!("redefition of enum '{}'", id).into(), location);
            }
        }
        let ctype = Type::Enum(enum_name, members);
        match &ctype {
            Type::Enum(_, members) => {
                for (id, _) in members {
                    self.scope.insert(
                        id.clone(),
                        Metadata {
                            id: *id,
                            storage_class: StorageClass::Register,
                            qualifiers: Qualifiers::NONE,
                            ctype: ctype.clone(),
                        }
                        .insert(),
                    );
                }
            }
            _ => unreachable!(),
        }
        ctype
    }
    fn forward_declaration(
        &mut self, kind: Keyword, ident: InternedStr, location: Location,
    ) -> Type {
        if kind == Keyword::Enum {
            // see section 6.7.2.3 of the C11 standard
            self.err(
                SemanticError::from(format!(
                    "cannot have forward reference to enum type '{}'",
                    ident
                )),
                location,
            );
            return Type::Enum(Some(ident), vec![]);
        }
        let struct_ref = StructRef::new();
        let (entry_type, tag_type): (fn(_) -> _, fn(_) -> _) = if kind == Keyword::Struct {
            (TagEntry::Struct, Type::Struct)
        } else {
            (TagEntry::Union, Type::Union)
        };
        let entry = entry_type(struct_ref);
        self.tag_scope.insert(ident, entry);
        tag_type(StructType::Named(ident, struct_ref))
    }
    /// Parse the declarator for a variable, given a starting type.
    /// e.g. for `int *p`, takes `start: Type::Int(true)` and returns `Type::Pointer(Type::Int(true))`
    fn parse_declarator(
        &mut self, current: Type, decl: ast::DeclaratorType, location: Location,
    ) -> Type {
        use crate::data::ast::DeclaratorType::*;
        use crate::data::types::{ArrayType, FunctionType};
        match decl {
            End => current,
            Pointer { to, qualifiers } => {
                use UnitSpecifier::*;

                let inner = self.parse_declarator(current, *to, location);
                let (counter, compounds) =
                    count_specifiers(qualifiers, &mut self.error_handler, location);
                let qualifiers = Qualifiers {
                    c_const: counter.get(&Const).is_some(),
                    volatile: counter.get(&Volatile).is_some(),
                    func: FunctionQualifiers {
                        inline: counter.get(&Inline).is_some(),
                        no_return: counter.get(&NoReturn).is_some(),
                    },
                };
                for &q in counter.keys() {
                    if !q.is_qualifier() {
                        self.err(SemanticError::NotAQualifier(q.into()), location);
                    }
                }
                for spec in compounds {
                    self.err(SemanticError::NotAQualifier(spec), location);
                }
                Type::Pointer(Box::new(inner), qualifiers)
            }
            Array { of, size } => {
                let size = if let Some(expr) = size {
                    let size =
                        Self::const_int(self.parse_expr(*expr), false).unwrap_or_else(|err| {
                            self.error_handler.push_back(err);
                            1
                        });
                    ArrayType::Fixed(size)
                } else {
                    ArrayType::Unbounded
                };
                let of = self.parse_declarator(current, *of, location);
                if let Type::Function(_) = &of {
                    self.err(SemanticError::ArrayStoringFunction(of.clone()), location);
                }
                Type::Array(Box::new(of), size)
            }
            Function(func) => {
                // TODO: give a warning for `const int f();` somewhere
                let return_type = self.parse_declarator(current, *func.return_type, location);
                match &return_type {
                    Type::Array(_, _) => self.err(
                        SemanticError::IllegalReturnType(return_type.clone()),
                        location,
                    ),
                    Type::Function(_) => self.err(
                        SemanticError::IllegalReturnType(return_type.clone()),
                        location,
                    ),
                    _ => {}
                }

                let mut names = HashSet::new();
                let mut params = Vec::new();
                for param in func.params {
                    // TODO: this location should be that of the param, not of the function
                    let mut param_type =
                        self.parse_type(param.specifiers, param.declarator.decl, location);

                    if let Type::Array(to, _) = param_type.ctype {
                        param_type.ctype = Type::Pointer(to, Qualifiers::default());
                    }

                    // C11 Standard 6.7.6.3 paragraph 8
                    // "A declaration of a parameter as 'function returning type' shall be
                    //  adjusted to 'pointer to function returning type', as in 6.3.2.1."
                    if param_type.ctype.is_function() {
                        param_type.ctype =
                            Type::Pointer(Box::new(param_type.ctype), Qualifiers::default());
                    }

                    if let Some(sc) = param_type.storage_class {
                        self.err(SemanticError::ParameterStorageClass(sc), location);
                    }
                    let id = if let Some(name) = param.declarator.id {
                        if names.contains(&name) {
                            self.err(SemanticError::DuplicateParameter(name), location)
                        }
                        names.insert(name);
                        name
                    } else {
                        InternedStr::default()
                    };
                    // TODO: `int f(int g())` should decay to `int f(int (*g)())`
                    let meta = Metadata {
                        ctype: param_type.ctype,
                        id,
                        qualifiers: param_type.qualifiers,
                        storage_class: StorageClass::Auto,
                    };
                    params.push(meta);
                }
                // int f(void);
                let is_void = match params.as_slice() {
                    [Metadata {
                        ctype: Type::Void, ..
                    }] => true,
                    _ => false,
                };
                // int f(void, int) or int f(int, void) or ...
                if !is_void
                    && params.iter().any(|param| match param.ctype {
                        Type::Void => true,
                        _ => false,
                    })
                {
                    self.err(SemanticError::InvalidVoidParameter, location);
                }
                Type::Function(FunctionType {
                    params: params.into_iter().map(|m| m.insert()).collect(),
                    return_type: Box::new(return_type),
                    varargs: func.varargs,
                })
            }
        }
    }
    // used for arrays like `int a[BUF_SIZE - 1];` and enums like `enum { A = 1 }`
    fn const_int(expr: Expr, allow_negative: bool) -> CompileResult<crate::arch::SIZE_T> {
        use Literal::*;

        let location = expr.location;
        let lit = expr.const_fold()?.into_literal().or_else(|runtime_expr| {
            Err(Locatable::new(
                SemanticError::NotConstant(runtime_expr),
                location,
            ))
        })?;
        match lit {
            UnsignedInt(i) => Ok(i),
            Int(i) => {
                if !allow_negative && i < 0 {
                    Err(Locatable::new(
                        SemanticError::NegativeLength.into(),
                        location,
                    ))
                } else {
                    Ok(i as u64)
                }
            }
            Char(c) => Ok(c.into()),
            Str(_) | Float(_) => Err(Locatable::new(
                SemanticError::NonIntegralLength.into(),
                location,
            )),
        }
    }
    fn declare(&mut self, symbol: MetadataRef, location: Location) {
        let decl = symbol.get();
        if decl.id == "main".into() {
            if let Type::Function(ftype) = &decl.ctype {
                if !ftype.is_main_func_signature() {
                    self.err(SemanticError::IllegalMainSignature, location);
                }
            }
        }
        let decl_init = self.initialized.contains(&symbol);
        // e.g. extern int i = 1;
        // this is a silly thing to do, but valid: https://stackoverflow.com/a/57900212/7669110
        if decl.storage_class == StorageClass::Extern && !decl.ctype.is_function() && decl_init {
            self.warn(Warning::ExtraneousExtern, location);
        }
        if let Some(existing_ref) = self.scope.insert(decl.id, symbol) {
            let existing = existing_ref.get();
            let meta = symbol.get();
            // 6.2.2p4
            // For an identifier declared with the storage-class specifier extern in a scope in which a prior declaration of that identifier is visible,
            // if the prior declaration specifies internal or external linkage,
            // the linkage of the identifier at the later declaration is the same as the linkage specified at the prior declaration.
            // If no prior declaration is visible, or if the prior declaration specifies no linkage, then the identifier has external linkage.
            //
            // i.e. `static int f(); int f();` is the same as `static int f(); static int f();`
            // special case redefining the same type
            if existing == meta
                || (existing.storage_class == StorageClass::Static
                    && meta.storage_class == StorageClass::Extern)
            {
                if decl_init && self.initialized.contains(&existing_ref) {
                    self.err(SemanticError::Redefinition(decl.id), location);
                }
            } else {
                let err = SemanticError::IncompatibleRedeclaration(decl.id, existing_ref, symbol);
                self.err(err, location);
            }
        }
    }
}

impl types::FunctionType {
    // check if this is a valid signature for 'main'
    fn is_main_func_signature(&self) -> bool {
        // main must return 'int' and must not be variadic
        if *self.return_type != Type::Int(true) || self.varargs {
            return false;
        }
        // allow 'main()''
        if self.params.is_empty() {
            return true;
        }
        // so the borrow-checker doesn't complain
        let meta: Vec<_> = self.params.iter().map(|param| param.get()).collect();
        let types: Vec<_> = meta.iter().map(|param| &param.ctype).collect();
        // allow 'main(void)'
        if types == [&Type::Void] {
            return true;
        }
        // TODO: allow 'int main(int argc, char *argv[], char *environ[])'
        if types.len() != 2 || *types[0] != Type::Int(true) {
            return false;
        }
        match types[1] {
            Type::Pointer(t, _) | Type::Array(t, _) => match &**t {
                Type::Pointer(inner, _) => inner.is_char(),
                _ => false,
            },
            _ => false,
        }
    }
}

impl Type {
    #[inline]
    fn is_char(&self) -> bool {
        match self {
            Type::Char(true) => true,
            _ => false,
        }
    }
}

struct FunctionAnalyzer<'a, T: Lexer> {
    /// the function we are currently compiling.
    /// used for checking return types
    metadata: FunctionData,
    /// We need this for the scopes, as well as for parsing expressions
    analyzer: &'a mut Analyzer<T>,
}

#[derive(Debug)]
/// used to keep track of function metadata
/// while doing semantic analysis
struct FunctionData {
    /// the name of the function
    id: InternedStr,
    /// where the function was declared
    location: Location,
    /// the return type of the function
    return_type: Type,
}

impl<'a, T: Lexer> FunctionAnalyzer<'a, T> {
    /// Performs semantic analysis on the function and adds it to `METADATA_STORE`.
    /// Returns the analyzed statements.
    fn analyze(
        func: ast::FunctionDefinition, analyzer: &mut Analyzer<T>, location: Location,
    ) -> (MetadataRef, Vec<Stmt>) {
        let parsed_func = analyzer.parse_type(func.specifiers, func.declarator.into(), location);
        if parsed_func.qualifiers != Qualifiers::default() {
            analyzer.error_handler.warn(
                Warning::FunctionQualifiersIgnored(parsed_func.qualifiers),
                location,
            );
        }
        let sc = match parsed_func.storage_class {
            None => StorageClass::Extern,
            Some(sc @ StorageClass::Extern) | Some(sc @ StorageClass::Static) => sc,
            Some(other) => {
                analyzer.err(SemanticError::InvalidFuncStorageClass(other), location);
                StorageClass::Extern
            }
        };
        let metadata = Metadata {
            // TODO: is it possible to remove this clone?
            // if we made params store `MetadataRef` instead of `Metadata`
            // we could use `func_type.params.iter()` instead of `into_iter()`.
            ctype: parsed_func.ctype.clone(),
            id: func.id,
            qualifiers: parsed_func.qualifiers,
            storage_class: sc,
        }
        .insert();
        let func_type = match parsed_func.ctype {
            Type::Function(ftype) => ftype,
            _ => unreachable!(),
        };
        let tmp_metadata = FunctionData {
            location,
            id: func.id,
            return_type: *func_type.return_type,
        };
        // TODO: add this function into the global scope
        assert!(analyzer.scope.is_global());
        assert!(analyzer.tag_scope.is_global());
        let mut func_analyzer = FunctionAnalyzer {
            metadata: tmp_metadata,
            analyzer,
        };
        func_analyzer.enter_scope();
        for (i, param) in func_type.params.into_iter().enumerate() {
            let meta = param.get();
            if meta.id == InternedStr::default() && meta.ctype != Type::Void {
                func_analyzer.err(
                    SemanticError::MissingParamName(i, meta.ctype.clone()),
                    location,
                );
            }
            func_analyzer.analyzer.scope.insert(meta.id, param);
        }
        let stmts = func
            .body
            .into_iter()
            .map(|s| func_analyzer.parse_stmt(s))
            .collect();
        // TODO: this should be the end of the function, not the start
        func_analyzer.leave_scope(location);
        assert!(analyzer.tag_scope.is_global());
        assert!(analyzer.scope.is_global());
        (metadata, stmts)
    }
}

impl<T: Lexer> FunctionAnalyzer<'_, T> {
    fn err(&mut self, err: SemanticError, location: Location) {
        self.analyzer.err(err, location);
    }
    fn enter_scope(&mut self) {
        self.analyzer.scope.enter();
        self.analyzer.tag_scope.enter();
    }
    fn leave_scope(&mut self, location: Location) {
        for object in self.analyzer.scope.get_all_immediate().values() {
            let object = object.get();
            match &object.ctype {
                Type::Struct(StructType::Named(name, members))
                | Type::Union(StructType::Named(name, members)) => {
                    if members.get().is_empty()
                        && object.storage_class != StorageClass::Extern
                        && object.storage_class != StorageClass::Typedef
                    {
                        self.analyzer.error_handler.error(
                            SemanticError::ForwardDeclarationIncomplete(*name, object.id),
                            location,
                        );
                    }
                }
                _ => {}
            }
        }
        self.analyzer.scope.exit();
        self.analyzer.tag_scope.exit();
    }
}

struct ParsedType {
    // needs to be option because the default varies greatly depending on the context
    storage_class: Option<StorageClass>,
    qualifiers: Qualifiers,
    ctype: Type,
    // TODO: this is fishy
    declared_compound_type: bool,
}

use ast::{DeclarationSpecifier, UnitSpecifier};

fn count_specifiers(
    specifiers: Vec<DeclarationSpecifier>, error_handler: &mut ErrorHandler, location: Location,
) -> (Counter<UnitSpecifier, usize>, Vec<DeclarationSpecifier>) {
    use DeclarationSpecifier::*;
    use UnitSpecifier::*;

    let mut counter = Counter::<_, usize>::new();
    let mut compounds = Vec::new();
    for spec in specifiers {
        match spec {
            Unit(u) => counter.update(std::iter::once(u)),
            _ => compounds.push(spec),
        }
    }
    for (&spec, &count) in counter.iter() {
        if spec != Long && count > 1 {
            if spec.is_type() {
                let err = SemanticError::InvalidSpecifier {
                    existing: spec.into(),
                    new: spec.into(),
                };
                error_handler.error(err, location);
            } else {
                error_handler.warn(Warning::DuplicateSpecifier(spec, count), location);
            }
        }
    }
    (counter, compounds)
}

impl UnitSpecifier {
    fn is_qualifier(self) -> bool {
        use UnitSpecifier::*;
        match self {
            Const | Volatile | Restrict | Inline | NoReturn => true,
            _ => false,
        }
    }
    /// Returns whether this is a self-contained type, not just whether this modifies a type.
    /// For example, `int` and `long` are self-contained types, but `unsigned` and `_Complex` are not.
    /// This is despite the fact that `unsigned i;` is valid and means `unsigned int i;`
    fn is_type(self) -> bool {
        use UnitSpecifier::*;
        match self {
            Bool | Char | Int | Long | Float | Double | VaList => true,
            _ => false,
        }
    }
}

#[cfg(test)]
pub(crate) mod test {
    use super::{Error, *};
    use crate::data::types::{ArrayType, FunctionType, Type::*};
    use crate::lex::PreProcessor;
    use crate::parse::test::*;

    pub(crate) fn analyze<'c, 'input: 'c, P, A, R, S, E>(
        input: &'input str, parse_func: P, analyze_func: A,
    ) -> CompileResult<R>
    where
        P: Fn(&mut Parser<PreProcessor<'c>>) -> Result<S, E>,
        A: Fn(&mut Analyzer<PreProcessor<'c>>, S) -> R,
        CompileError: From<E>,
    {
        let mut p = parser(input);
        let ast = parse_func(&mut p)?;
        let mut a = Analyzer::new(p);
        let e = analyze_func(&mut a, ast);
        if let Some(err) = a.error_handler.pop_front() {
            return Err(err);
        }
        Ok(e)
    }

    fn maybe_decl(s: &str) -> Option<CompileResult<Declaration>> {
        decls(s).into_iter().next()
    }

    pub(crate) fn decl(s: &str) -> CompileResult<Declaration> {
        maybe_decl(s).unwrap_or_else(|| panic!("expected a declaration or error: '{}'", s))
    }

    pub(crate) fn decls(s: &str) -> Vec<CompileResult<Declaration>> {
        Analyzer::new(parser(s))
            .map(|o| o.map(|l| l.data))
            .collect()
    }

    pub(crate) fn assert_errs_decls(input: &str, errs: usize, warnings: usize, decls: usize) {
        let mut a = Analyzer::new(parser(input));
        let (mut a_errs, mut a_decls) = (0, 0);
        for res in &mut a {
            if res.is_err() {
                a_errs += 1;
            } else {
                a_decls += 1;
            }
        }
        let a_warns = a.error_handler.warnings.len();

        if (a_errs, a_warns, a_decls) != (errs, warnings, decls) {
            println!(
                "({} errs, {} warnings, {} decls) != ({}, {}, {}) when parsing {}",
                a_errs, a_warns, a_decls, errs, warnings, decls, input
            );
            println!("note: warnings:");
            for warning in a.error_handler.warnings {
                println!("- {}", warning.data);
            }
        };
    }

    pub(crate) fn analyze_expr(s: &str) -> CompileResult<Expr> {
        analyze(s, Parser::expr, Analyzer::parse_expr)
    }

    fn assert_decl_display(left: &str, right: &str) {
        assert_eq!(decl(left).unwrap().to_string(), right);
    }
    fn assert_extern_decl_display(s: &str) {
        // TODO: this `auto` is such a hack
        assert_decl_display(s, &format!("auto {}", s));
    }

    pub(super) fn assert_same(left: &str, right: &str) {
        assert_eq!(
            decl(left).unwrap().to_string(),
            decl(right).unwrap().to_string()
        );
    }
    fn assert_no_change(s: &str) {
        assert_decl_display(s, s);
    }

    fn match_type(lexed: CompileResult<Declaration>, given_type: Type) -> bool {
        fn type_helper(ctype: &Type, given_type: &Type) -> bool {
            match (ctype, given_type) {
                // because the parameters use `MetadataRef`,
                // it's impossible to have the same ref twice, even in unit tess
                (Type::Function(actual), Type::Function(expected)) => {
                    // TODO: this only handles one level of function nesting
                    actual
                        .params
                        .iter()
                        .zip(&expected.params)
                        .all(|(left, right)| metadata_helper(&left.get(), &right.get()))
                        && {
                            println!("all params match");
                            true
                        }
                        && dbg!(type_helper(&actual.return_type, &expected.return_type))
                        && dbg!(actual.varargs == expected.varargs)
                }
                (Type::Pointer(a, lq), Type::Pointer(b, rq)) => type_helper(&*a, &*b) && lq == rq,
                (Type::Array(a, la), Type::Array(b, ra)) => type_helper(&*a, &*b) && la == ra,
                (a, b) => a == b,
            }
        }
        fn metadata_helper(left: &Metadata, right: &Metadata) -> bool {
            dbg!(type_helper(dbg!(&left.ctype), dbg!(&right.ctype)))
                && left.storage_class == right.storage_class
                && left.qualifiers == right.qualifiers
                && left.id == right.id
        }
        lexed.map_or(false, |decl| {
            type_helper(&decl.symbol.get().ctype, &given_type)
        })
    }

    #[test]
    fn no_name_should_be_syntax_error() {
        match decl("int *;").unwrap_err().data {
            Error::Syntax(_) => {}
            _ => panic!("expected syntax error"),
        }
    }

    #[test]
    fn storage_class() {
        assert_extern_decl_display("int i;");
        assert_eq!(
            decl("extern int i;").unwrap().symbol.get().storage_class,
            StorageClass::Extern
        );
        assert_eq!(
            decl("static int i;").unwrap().symbol.get().storage_class,
            StorageClass::Static
        );
        match decl("auto int i;").unwrap_err().data {
            Error::Semantic(SemanticError::AutoAtGlobalScope) => {}
            _ => panic!("wrong error"),
        }
    }

    #[test]
    fn function() {
        assert_extern_decl_display("int f();");
        assert_extern_decl_display("int f(int i);");
        assert_extern_decl_display("int f(int i, int j);");
        // functions decay to pointers when used as parameters
        assert_same("int f(int g());", "int f(int (*g)());");
        assert_same("int f(int g(), ...);", "int f(int (*g)(), ...);");
    }

    #[test]
    fn test_decl_specifiers() {
        assert!(match_type(decl("char i;"), Type::Char(true)));
        assert!(match_type(decl("unsigned char i;"), Type::Char(false)));
        assert!(match_type(decl("signed short i;"), Type::Short(true)));
        assert!(match_type(decl("unsigned short i;"), Type::Short(false)));
        assert!(match_type(decl("long i;"), Type::Long(true)));
        assert!(match_type(decl("long long i;"), Type::Long(true)));
        assert!(match_type(decl("long unsigned i;"), Type::Long(false)));
        assert!(match_type(decl("int i;"), Type::Int(true)));
        assert!(match_type(decl("signed i;"), Type::Int(true)));
        assert!(match_type(decl("unsigned i;"), Type::Int(false)));
        assert!(match_type(decl("float f;"), Type::Float));
        assert!(match_type(decl("double d;"), Type::Double));
        assert!(match_type(decl("long double d;"), Type::Double));
        assert!(match_type(
            decl("void f();"),
            Type::Function(FunctionType {
                return_type: Box::new(Type::Void),
                params: vec![],
                varargs: false
            })
        ));
        assert!(match_type(decl("const volatile int f;"), Type::Int(true)));
    }
    #[test]
    fn test_bad_decl_specs() {
        assert!(maybe_decl("int;").is_none());
        for s in &[
            "char char i;",
            "char long i;",
            "long char i;",
            "float char i;",
            "float double i;",
            "double double i;",
            "double unsigned i;",
            "short double i;",
            "int void i;",
            "void int i;",
        ] {
            assert!(decl(s).is_err(), "'{}' should be an error", s);
        }
        // default to int if we don't have a type
        // don't panic if we see duplicate specifiers
        assert!(match_type(decl("unsigned unsigned i;"), Type::Int(false)));
        assert!(match_type(decl("extern extern i;"), Type::Int(true)));
        assert!(match_type(decl("const const i;"), Type::Int(true)));
        assert!(match_type(decl("const volatile i;"), Type::Int(true)));
    }
    #[test]
    fn test_arrays() {
        assert!(match_type(
            decl("int a[];"),
            Array(Box::new(Int(true)), ArrayType::Unbounded)
        ));
        assert!(match_type(
            decl("unsigned a[];"),
            Array(Box::new(Int(false)), ArrayType::Unbounded)
        ));
        assert!(match_type(
            decl("_Bool a[][][];"),
            Array(
                Box::new(Array(
                    Box::new(Array(Box::new(Bool), ArrayType::Unbounded)),
                    ArrayType::Unbounded
                )),
                ArrayType::Unbounded
            )
        ));
        assert_extern_decl_display("int a[1];");
        assert_same("int a[(int)1];", "int a[1];");
    }
    #[test]
    fn test_pointers() {
        for &pointer in &[
            "void *a;",
            "float *const a;",
            "double *volatile *const a;",
            "double *volatile *const a;",
            "_Bool *const volatile a;",
        ] {
            assert_extern_decl_display(pointer);
        }
    }
    #[test]
    fn test_pointers_and_arrays() {
        // cdecl: declare foo as array 10 of pointer to pointer to char
        assert!(match_type(
            decl("char **foo[10];"),
            Array(
                Box::new(Pointer(
                    Box::new(Pointer(Box::new(Char(true)), Qualifiers::default(),)),
                    Qualifiers::default(),
                )),
                ArrayType::Fixed(10),
            )
        ));
        // cdecl: declare foo as pointer to pointer to array 10 of int
        assert!(match_type(
            decl("int (**foo)[10];"),
            Pointer(
                Box::new(Pointer(
                    Box::new(Array(Box::new(Int(true)), ArrayType::Fixed(10),)),
                    Qualifiers::default(),
                )),
                Qualifiers::default(),
            )
        ));
    }
    #[test]
    fn test_functions() {
        assert!(match_type(
            decl("void *f();"),
            Function(FunctionType {
                return_type: Box::new(Pointer(Box::new(Type::Void), Qualifiers::default())),
                params: vec![],
                varargs: false,
            })
        ));
        // cdecl: declare i as pointer to function returning int;
        assert!(match_type(
            decl("int (*i)();"),
            Pointer(
                Box::new(Function(FunctionType {
                    return_type: Box::new(Int(true)),
                    params: vec![],
                    varargs: false,
                })),
                Qualifiers::default()
            )
        ));
        // cdecl: declare i as pointer to function (int, char, float) returning int
        assert_no_change("extern int (*i)(int, char, float);");
        // cdecl: declare i as pointer to function (pointer to function returning int) returning int
        assert!(match_type(
            decl("int (*i)(int (*f)());"),
            Pointer(
                Box::new(Function(FunctionType {
                    return_type: Box::new(Int(true)),
                    params: vec![Metadata {
                        id: InternedStr::get_or_intern("f"),
                        ctype: Pointer(
                            Box::new(Function(FunctionType {
                                return_type: Box::new(Int(true)),
                                params: vec![],
                                varargs: false
                            })),
                            Qualifiers::default()
                        ),
                        qualifiers: Default::default(),
                        storage_class: Default::default(),
                    }
                    .insert()],
                    varargs: false,
                })),
                Qualifiers::default()
            )
        ));
        assert!(match_type(
            decl("int f(int, ...);"),
            Function(FunctionType {
                return_type: Box::new(Int(true)),
                params: vec![Metadata {
                    id: Default::default(),
                    ctype: Int(true),
                    qualifiers: Default::default(),
                    storage_class: Default::default()
                }
                .insert()],
                varargs: true,
            })
        ));
    }
    #[test]
    fn test_functions_array_parameter_static() {
        assert!(match_type(
            decl("void f(int a[static 5]);"),
            Function(FunctionType {
                return_type: Box::new(Void),
                params: vec![Metadata {
                    id: InternedStr::get_or_intern("a"),
                    ctype: Pointer(Box::new(Int(true)), Qualifiers::default()),
                    qualifiers: Default::default(),
                    storage_class: Default::default(),
                }
                .insert()],
                varargs: false
            })
        ));

        assert!(decl("int b[static 10];").is_err());
    }
    #[test]
    fn test_inline_keyword() {
        // Correct usage
        assert!(match_type(
            decl("inline void f(void);"),
            Function(FunctionType {
                return_type: Box::new(Void),
                params: vec![Metadata {
                    id: InternedStr::default(),
                    ctype: Type::Void,
                    qualifiers: Qualifiers::default(),
                    storage_class: StorageClass::default(),
                }
                .insert()],
                varargs: false,
            })
        ));

        // `inline` is not allowed in the following cases
        assert!(decl("inline int a;").is_err()); // Normal declarations
        assert!(decl("void f(inline int a);").is_err()); // Parameter lists
        assert!(decl("struct F { inline int a; } f;").is_err()); // Struct members
        assert!(
            // Type names
            decl("int main() { char a = (inline char)(4); }").is_err()
        );
        assert!(decl("typedef a inline int;").is_err());
    }
    #[test]
    fn test_complex() {
        // cdecl: declare bar as const pointer to array 10 of pointer to function (int) returning volatile pointer to char
        assert!(match_type(
            decl("char * volatile (*(* const bar)[])(int );"),
            Pointer(
                Box::new(Array(
                    Box::new(Pointer(
                        Box::new(Function(FunctionType {
                            return_type: Box::new(Pointer(
                                Box::new(Char(true)),
                                Qualifiers {
                                    volatile: true,
                                    ..Qualifiers::default()
                                }
                            )),
                            params: vec![Metadata {
                                ctype: Int(true),
                                storage_class: Default::default(),
                                id: Default::default(),
                                qualifiers: Qualifiers::NONE,
                            }
                            .insert()],
                            varargs: false,
                        })),
                        Qualifiers::default()
                    )),
                    ArrayType::Unbounded,
                )),
                Qualifiers {
                    c_const: true,
                    ..Qualifiers::default()
                }
            )
        ));
        // cdecl: declare foo as pointer to function (void) returning pointer to array 3 of int
        assert!(match_type(
            decl("int (*(*foo)(void))[];"),
            Pointer(
                Box::new(Function(FunctionType {
                    return_type: Box::new(Pointer(
                        Box::new(Array(Box::new(Int(true)), ArrayType::Unbounded)),
                        Qualifiers::default()
                    )),
                    params: vec![Metadata {
                        ctype: Void,
                        storage_class: Default::default(),
                        id: Default::default(),
                        qualifiers: Default::default(),
                    }
                    .insert()],
                    varargs: false,
                })),
                Qualifiers::default()
            )
        ));
        // cdecl: declare bar as volatile pointer to array 64 of const int
        assert!(match_type(
            decl("const int (* volatile bar)[];"),
            Pointer(
                Box::new(Array(Box::new(Int(true)), ArrayType::Unbounded)),
                Qualifiers {
                    volatile: true,
                    ..Qualifiers::default()
                }
            )
        ));
        // cdecl: declare x as function returning pointer to array 5 of pointer to function returning char
        assert!(match_type(
            decl("char (*(*x())[])();"),
            Function(FunctionType {
                return_type: Box::new(Pointer(
                    Box::new(Array(
                        Box::new(Pointer(
                            Box::new(Function(FunctionType {
                                return_type: Box::new(Char(true)),
                                params: vec![],
                                varargs: false,
                            })),
                            Qualifiers::default()
                        )),
                        ArrayType::Unbounded
                    )),
                    Qualifiers::default()
                )),
                params: vec![],
                varargs: false,
            })
        ));
    }
    #[test]
    fn test_multiple() {
        assert_same("int i, j, k;", "int i; int j; int k;");
        assert_same(
            "char *p, c, **pp, f();",
            "char *p; char c; char **p; char f();",
        );
    }
    #[test]
    fn test_no_specifiers() {
        assert_same("i, j, k;", "int i, j, k;");
        assert_same("*p, c, **pp, f();", "int *p, c, **pp, f();");
    }
    #[test]
    fn test_decl_errors() {
        // no semicolon
        assert!(decl("int").is_err());
        assert!(decl("int i").is_err());
        // type error: cannot have array of functions or function returning array
        assert!(decl("int f()[];").is_err());
        assert!(decl("int f[]();").is_err());
        assert!(decl("int f()();").is_err());
        assert!(decl("int (*f)[;").is_err());
        // duplicate parameter name
        assert!(decl("int f(int a, int a);").is_err());
    }
    #[test]
    fn default_type_specifier_warns() {
        let default_type_decls = &[
            "i;",
            "f();",
            "a[1];",
            "(*fp)();",
            "(i);",
            "((*f)());",
            "(a[1]);",
            "(((((((((i)))))))));",
        ];

        for decl in default_type_decls {
            assert_errs_decls(decl, 0, 1, 1);
        }
    }

    #[test]
    fn extern_redeclaration_of_static_fn_does_not_error() {
        assert_same(
            "static int f(); int f();",
            "static int f(); extern int f();",
        );

        // However the opposite should still error
        assert_errs_decls(
            "extern int f();
                static int f();",
            1,
            0,
            2,
        );
    }

    #[test]
    fn enum_declaration() {
        assert!(decl("enum;").is_err());
        assert!(decl("enum e;").is_err());
        assert!(decl("enum e {};").is_err());
        assert!(decl("enum e { A }").is_err());
        assert!(maybe_decl("enum { A };").is_none());
        assert!(match_type(
            decl("enum { A } E;"),
            Type::Enum(None, vec![("A".into(), 0)])
        ));
        assert!(match_type(
            decl("enum e { A = 1, B } E;"),
            Type::Enum(Some("e".into()), vec![("A".into(), 1), ("B".into(), 2)])
        ));
        assert!(match_type(
            decl("enum { A = -5, B, C = 2, D } E;"),
            Type::Enum(
                None,
                vec![
                    ("A".into(), -5),
                    ("B".into(), -4),
                    ("C".into(), 2),
                    ("D".into(), 3)
                ]
            )
        ));
    }

    #[test]
    fn typedef_signed() {
        let mut ds = decls("typedef unsigned uint; uint i;").into_iter();
        assert_eq!(
            ds.next().unwrap().unwrap().to_string(),
            "typedef unsigned int uint;"
        );
        assert_decl_display("unsigned int i;", &ds.next().unwrap().unwrap().to_string());
    }
    #[test]
    fn bitfields() {
        assert!(decl("struct { int:5; } a;").is_err());
        assert!(decl("struct { int a:5; } b;").is_ok());
        assert!(decl("struct { int a:5, b:6; } c;").is_ok());
        assert!(decl("struct { extern int a:5; } d;").is_err());
    }
    #[test]
    fn lol() {
        let lol = "
int *jynelson(int(*fp)(int)) {
    return 0;
}
int f(int i) {
    return 0;
}
int main() {
    return *((int*(*)(int(*)(int)))jynelson)(&f);
}
";
        assert!(parse_all(lol).iter().all(Result::is_ok));
    }
    #[test]
    fn redefinition_is_err() {
        assert_errs_decls("int i = 1, i = 2;", 1, 0, 2);
    }
    #[test]
    fn void() {
        assert_no_change("extern int f(void);");
        assert_no_change("extern int f(int);");
        assert!(decl("int f(int, void);").is_err());
        assert!(decl("int f(void, int);").is_err());
        assert!(decl("int f(void, void);").is_err());
        assert!(decl("int f(int) { return 1; }").is_err());
        assert_decl_display(
            "int f(void) { return 1; }",
            "extern int f(void) {\n    return (int)(1);\n}\n",
        );
    }
}