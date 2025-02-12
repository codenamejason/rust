// Copyright 2012 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use ast;
use codemap::{Span, respan};
use ext::base::*;
use ext::base;
use ext::build::AstBuilder;
use fmt_macros as parse;
use parse::token::InternedString;
use parse::token;
use ptr::P;

use std::collections::HashMap;
use std::string;

#[deriving(PartialEq)]
enum ArgumentType {
    Known(string::String),
    Unsigned
}

enum Position {
    Exact(uint),
    Named(string::String),
}

struct Context<'a, 'b:'a> {
    ecx: &'a mut ExtCtxt<'b>,
    fmtsp: Span,

    /// Parsed argument expressions and the types that we've found so far for
    /// them.
    args: Vec<P<ast::Expr>>,
    arg_types: Vec<Option<ArgumentType>>,
    /// Parsed named expressions and the types that we've found for them so far.
    /// Note that we keep a side-array of the ordering of the named arguments
    /// found to be sure that we can translate them in the same order that they
    /// were declared in.
    names: HashMap<string::String, P<ast::Expr>>,
    name_types: HashMap<string::String, ArgumentType>,
    name_ordering: Vec<string::String>,

    /// The latest consecutive literal strings, or empty if there weren't any.
    literal: string::String,

    /// Collection of the compiled `rt::Argument` structures
    pieces: Vec<P<ast::Expr>>,
    /// Collection of string literals
    str_pieces: Vec<P<ast::Expr>>,
    /// Stays `true` if all formatting parameters are default (as in "{}{}").
    all_pieces_simple: bool,

    name_positions: HashMap<string::String, uint>,
    method_statics: Vec<P<ast::Item>>,

    /// Updated as arguments are consumed or methods are entered
    nest_level: uint,
    next_arg: uint,
}

pub enum Invocation {
    Call(P<ast::Expr>),
    MethodCall(P<ast::Expr>, ast::Ident),
}

/// Parses the arguments from the given list of tokens, returning None
/// if there's a parse error so we can continue parsing other format!
/// expressions.
///
/// If parsing succeeds, the second return value is:
///
///     Some((fmtstr, unnamed arguments, ordering of named arguments,
///           named arguments))
fn parse_args(ecx: &mut ExtCtxt, sp: Span, allow_method: bool,
              tts: &[ast::TokenTree])
    -> (Invocation, Option<(P<ast::Expr>, Vec<P<ast::Expr>>, Vec<string::String>,
                            HashMap<string::String, P<ast::Expr>>)>) {
    let mut args = Vec::new();
    let mut names = HashMap::<string::String, P<ast::Expr>>::new();
    let mut order = Vec::new();

    let mut p = ecx.new_parser_from_tts(tts);
    // Parse the leading function expression (maybe a block, maybe a path)
    let invocation = if allow_method {
        let e = p.parse_expr();
        if !p.eat(&token::Comma) {
            ecx.span_err(sp, "expected token: `,`");
            return (Call(e), None);
        }
        MethodCall(e, p.parse_ident())
    } else {
        Call(p.parse_expr())
    };
    if !p.eat(&token::Comma) {
        ecx.span_err(sp, "expected token: `,`");
        return (invocation, None);
    }

    if p.token == token::Eof {
        ecx.span_err(sp, "requires at least a format string argument");
        return (invocation, None);
    }
    let fmtstr = p.parse_expr();
    let mut named = false;
    while p.token != token::Eof {
        if !p.eat(&token::Comma) {
            ecx.span_err(sp, "expected token: `,`");
            return (invocation, None);
        }
        if p.token == token::Eof { break } // accept trailing commas
        if named || (p.token.is_ident() && p.look_ahead(1, |t| *t == token::Eq)) {
            named = true;
            let ident = match p.token {
                token::Ident(i, _) => {
                    p.bump();
                    i
                }
                _ if named => {
                    ecx.span_err(p.span,
                                 "expected ident, positional arguments \
                                 cannot follow named arguments");
                    return (invocation, None);
                }
                _ => {
                    ecx.span_err(p.span,
                                 format!("expected ident for named argument, found `{}`",
                                         p.this_token_to_string()).as_slice());
                    return (invocation, None);
                }
            };
            let interned_name = token::get_ident(ident);
            let name = interned_name.get();
            p.expect(&token::Eq);
            let e = p.parse_expr();
            match names.find_equiv(name) {
                None => {}
                Some(prev) => {
                    ecx.span_err(e.span,
                                 format!("duplicate argument named `{}`",
                                         name).as_slice());
                    ecx.parse_sess.span_diagnostic.span_note(prev.span, "previously here");
                    continue
                }
            }
            order.push(name.to_string());
            names.insert(name.to_string(), e);
        } else {
            args.push(p.parse_expr());
        }
    }
    return (invocation, Some((fmtstr, args, order, names)));
}

impl<'a, 'b> Context<'a, 'b> {
    /// Verifies one piece of a parse string. All errors are not emitted as
    /// fatal so we can continue giving errors about this and possibly other
    /// format strings.
    fn verify_piece(&mut self, p: &parse::Piece) {
        match *p {
            parse::String(..) => {}
            parse::NextArgument(ref arg) => {
                // width/precision first, if they have implicit positional
                // parameters it makes more sense to consume them first.
                self.verify_count(arg.format.width);
                self.verify_count(arg.format.precision);

                // argument second, if it's an implicit positional parameter
                // it's written second, so it should come after width/precision.
                let pos = match arg.position {
                    parse::ArgumentNext => {
                        let i = self.next_arg;
                        if self.check_positional_ok() {
                            self.next_arg += 1;
                        }
                        Exact(i)
                    }
                    parse::ArgumentIs(i) => Exact(i),
                    parse::ArgumentNamed(s) => Named(s.to_string()),
                };

                let ty = Known(arg.format.ty.to_string());
                self.verify_arg_type(pos, ty);
            }
        }
    }

    fn verify_count(&mut self, c: parse::Count) {
        match c {
            parse::CountImplied | parse::CountIs(..) => {}
            parse::CountIsParam(i) => {
                self.verify_arg_type(Exact(i), Unsigned);
            }
            parse::CountIsName(s) => {
                self.verify_arg_type(Named(s.to_string()), Unsigned);
            }
            parse::CountIsNextParam => {
                if self.check_positional_ok() {
                    let next_arg = self.next_arg;
                    self.verify_arg_type(Exact(next_arg), Unsigned);
                    self.next_arg += 1;
                }
            }
        }
    }

    fn check_positional_ok(&mut self) -> bool {
        if self.nest_level != 0 {
            self.ecx.span_err(self.fmtsp, "cannot use implicit positional \
                                           arguments nested inside methods");
            false
        } else {
            true
        }
    }

    fn describe_num_args(&self) -> string::String {
        match self.args.len() {
            0 => "no arguments given".to_string(),
            1 => "there is 1 argument".to_string(),
            x => format!("there are {} arguments", x),
        }
    }

    fn verify_arg_type(&mut self, arg: Position, ty: ArgumentType) {
        match arg {
            Exact(arg) => {
                if self.args.len() <= arg {
                    let msg = format!("invalid reference to argument `{}` ({:s})",
                                      arg, self.describe_num_args());

                    self.ecx.span_err(self.fmtsp, msg.as_slice());
                    return;
                }
                {
                    let arg_type = match self.arg_types[arg] {
                        None => None,
                        Some(ref x) => Some(x)
                    };
                    self.verify_same(self.args[arg].span, &ty, arg_type);
                }
                if self.arg_types[arg].is_none() {
                    self.arg_types[arg] = Some(ty);
                }
            }

            Named(name) => {
                let span = match self.names.find(&name) {
                    Some(e) => e.span,
                    None => {
                        let msg = format!("there is no argument named `{}`", name);
                        self.ecx.span_err(self.fmtsp, msg.as_slice());
                        return;
                    }
                };
                self.verify_same(span, &ty, self.name_types.find(&name));
                if !self.name_types.contains_key(&name) {
                    self.name_types.insert(name.clone(), ty);
                }
                // Assign this named argument a slot in the arguments array if
                // it hasn't already been assigned a slot.
                if !self.name_positions.contains_key(&name) {
                    let slot = self.name_positions.len();
                    self.name_positions.insert(name, slot);
                }
            }
        }
    }

    /// When we're keeping track of the types that are declared for certain
    /// arguments, we assume that `None` means we haven't seen this argument
    /// yet, `Some(None)` means that we've seen the argument, but no format was
    /// specified, and `Some(Some(x))` means that the argument was declared to
    /// have type `x`.
    ///
    /// Obviously `Some(Some(x)) != Some(Some(y))`, but we consider it true
    /// that: `Some(None) == Some(Some(x))`
    fn verify_same(&self,
                   sp: Span,
                   ty: &ArgumentType,
                   before: Option<&ArgumentType>) {
        let cur = match before {
            None => return,
            Some(t) => t,
        };
        if *ty == *cur {
            return
        }
        match (cur, ty) {
            (&Known(ref cur), &Known(ref ty)) => {
                self.ecx.span_err(sp,
                                  format!("argument redeclared with type `{}` when \
                                           it was previously `{}`",
                                          *ty,
                                          *cur).as_slice());
            }
            (&Known(ref cur), _) => {
                self.ecx.span_err(sp,
                                  format!("argument used to format with `{}` was \
                                           attempted to not be used for formatting",
                                           *cur).as_slice());
            }
            (_, &Known(ref ty)) => {
                self.ecx.span_err(sp,
                                  format!("argument previously used as a format \
                                           argument attempted to be used as `{}`",
                                           *ty).as_slice());
            }
            (_, _) => {
                self.ecx.span_err(sp, "argument declared with multiple formats");
            }
        }
    }

    /// These attributes are applied to all statics that this syntax extension
    /// will generate.
    fn static_attrs(ecx: &ExtCtxt, fmtsp: Span) -> Vec<ast::Attribute> {
        // Flag statics as `inline` so LLVM can merge duplicate globals as much
        // as possible (which we're generating a whole lot of).
        let unnamed = ecx.meta_word(fmtsp, InternedString::new("inline"));
        let unnamed = ecx.attribute(fmtsp, unnamed);

        // Do not warn format string as dead code
        let dead_code = ecx.meta_word(fmtsp, InternedString::new("dead_code"));
        let allow_dead_code = ecx.meta_list(fmtsp,
                                            InternedString::new("allow"),
                                            vec![dead_code]);
        let allow_dead_code = ecx.attribute(fmtsp, allow_dead_code);
        vec![unnamed, allow_dead_code]
    }

    fn rtpath(ecx: &ExtCtxt, s: &str) -> Vec<ast::Ident> {
        vec![ecx.ident_of("std"), ecx.ident_of("fmt"), ecx.ident_of("rt"), ecx.ident_of(s)]
    }

    fn trans_count(&self, c: parse::Count) -> P<ast::Expr> {
        let sp = self.fmtsp;
        match c {
            parse::CountIs(i) => {
                self.ecx.expr_call_global(sp, Context::rtpath(self.ecx, "CountIs"),
                                          vec!(self.ecx.expr_uint(sp, i)))
            }
            parse::CountIsParam(i) => {
                self.ecx.expr_call_global(sp, Context::rtpath(self.ecx, "CountIsParam"),
                                          vec!(self.ecx.expr_uint(sp, i)))
            }
            parse::CountImplied => {
                let path = self.ecx.path_global(sp, Context::rtpath(self.ecx,
                                                                    "CountImplied"));
                self.ecx.expr_path(path)
            }
            parse::CountIsNextParam => {
                let path = self.ecx.path_global(sp, Context::rtpath(self.ecx,
                                                                    "CountIsNextParam"));
                self.ecx.expr_path(path)
            }
            parse::CountIsName(n) => {
                let i = match self.name_positions.find_equiv(n) {
                    Some(&i) => i,
                    None => 0, // error already emitted elsewhere
                };
                let i = i + self.args.len();
                self.ecx.expr_call_global(sp, Context::rtpath(self.ecx, "CountIsParam"),
                                          vec!(self.ecx.expr_uint(sp, i)))
            }
        }
    }

    /// Translate the accumulated string literals to a literal expression
    fn trans_literal_string(&mut self) -> P<ast::Expr> {
        let sp = self.fmtsp;
        let s = token::intern_and_get_ident(self.literal.as_slice());
        self.literal.clear();
        self.ecx.expr_str(sp, s)
    }

    /// Translate a `parse::Piece` to a static `rt::Argument` or append
    /// to the `literal` string.
    fn trans_piece(&mut self, piece: &parse::Piece) -> Option<P<ast::Expr>> {
        let sp = self.fmtsp;
        match *piece {
            parse::String(s) => {
                self.literal.push_str(s);
                None
            }
            parse::NextArgument(ref arg) => {
                // Translate the position
                let pos = match arg.position {
                    // These two have a direct mapping
                    parse::ArgumentNext => {
                        let path = self.ecx.path_global(sp, Context::rtpath(self.ecx,
                                                                            "ArgumentNext"));
                        self.ecx.expr_path(path)
                    }
                    parse::ArgumentIs(i) => {
                        self.ecx.expr_call_global(sp, Context::rtpath(self.ecx, "ArgumentIs"),
                                                  vec!(self.ecx.expr_uint(sp, i)))
                    }
                    // Named arguments are converted to positional arguments at
                    // the end of the list of arguments
                    parse::ArgumentNamed(n) => {
                        let i = match self.name_positions.find_equiv(n) {
                            Some(&i) => i,
                            None => 0, // error already emitted elsewhere
                        };
                        let i = i + self.args.len();
                        self.ecx.expr_call_global(sp, Context::rtpath(self.ecx, "ArgumentIs"),
                                                  vec!(self.ecx.expr_uint(sp, i)))
                    }
                };

                let simple_arg = parse::Argument {
                    position: parse::ArgumentNext,
                    format: parse::FormatSpec {
                        fill: arg.format.fill,
                        align: parse::AlignUnknown,
                        flags: 0,
                        precision: parse::CountImplied,
                        width: parse::CountImplied,
                        ty: arg.format.ty
                    }
                };

                let fill = match arg.format.fill { Some(c) => c, None => ' ' };

                if *arg != simple_arg || fill != ' ' {
                    self.all_pieces_simple = false;
                }

                // Translate the format
                let fill = self.ecx.expr_lit(sp, ast::LitChar(fill));
                let align = match arg.format.align {
                    parse::AlignLeft => {
                        self.ecx.path_global(sp, Context::rtpath(self.ecx, "AlignLeft"))
                    }
                    parse::AlignRight => {
                        self.ecx.path_global(sp, Context::rtpath(self.ecx, "AlignRight"))
                    }
                    parse::AlignCenter => {
                        self.ecx.path_global(sp, Context::rtpath(self.ecx, "AlignCenter"))
                    }
                    parse::AlignUnknown => {
                        self.ecx.path_global(sp, Context::rtpath(self.ecx, "AlignUnknown"))
                    }
                };
                let align = self.ecx.expr_path(align);
                let flags = self.ecx.expr_uint(sp, arg.format.flags);
                let prec = self.trans_count(arg.format.precision);
                let width = self.trans_count(arg.format.width);
                let path = self.ecx.path_global(sp, Context::rtpath(self.ecx, "FormatSpec"));
                let fmt = self.ecx.expr_struct(sp, path, vec!(
                    self.ecx.field_imm(sp, self.ecx.ident_of("fill"), fill),
                    self.ecx.field_imm(sp, self.ecx.ident_of("align"), align),
                    self.ecx.field_imm(sp, self.ecx.ident_of("flags"), flags),
                    self.ecx.field_imm(sp, self.ecx.ident_of("precision"), prec),
                    self.ecx.field_imm(sp, self.ecx.ident_of("width"), width)));

                let path = self.ecx.path_global(sp, Context::rtpath(self.ecx, "Argument"));
                Some(self.ecx.expr_struct(sp, path, vec!(
                    self.ecx.field_imm(sp, self.ecx.ident_of("position"), pos),
                    self.ecx.field_imm(sp, self.ecx.ident_of("format"), fmt))))
            }
        }
    }

    fn item_static_array(ecx: &mut ExtCtxt,
                         name: ast::Ident,
                         piece_ty: P<ast::Ty>,
                         pieces: Vec<P<ast::Expr>>)
                         -> P<ast::Stmt> {
        let fmtsp = piece_ty.span;
        let pieces_len = ecx.expr_uint(fmtsp, pieces.len());
        let fmt = ecx.expr_vec(fmtsp, pieces);
        let ty = ast::TyFixedLengthVec(
            piece_ty,
            pieces_len
        );
        let ty = ecx.ty(fmtsp, ty);
        let st = ast::ItemStatic(ty, ast::MutImmutable, fmt);
        let item = ecx.item(fmtsp, name, Context::static_attrs(ecx, fmtsp), st);
        let decl = respan(fmtsp, ast::DeclItem(item));
        P(respan(fmtsp, ast::StmtDecl(P(decl), ast::DUMMY_NODE_ID)))
    }

    /// Actually builds the expression which the iformat! block will be expanded
    /// to
    fn to_expr(mut self, invocation: Invocation) -> P<ast::Expr> {
        let mut lets = Vec::new();
        let mut locals = Vec::new();
        let mut names = Vec::from_fn(self.name_positions.len(), |_| None);
        let mut pats = Vec::new();
        let mut heads = Vec::new();

        // First, declare all of our methods that are statics
        for method in self.method_statics.into_iter() {
            let decl = respan(self.fmtsp, ast::DeclItem(method));
            lets.push(P(respan(self.fmtsp,
                               ast::StmtDecl(P(decl), ast::DUMMY_NODE_ID))));
        }

        // Next, build up the static array which will become our precompiled
        // format "string"
        let static_str_name = self.ecx.ident_of("__STATIC_FMTSTR");
        let static_lifetime = self.ecx.lifetime(self.fmtsp, self.ecx.ident_of("'static").name);
        let piece_ty = self.ecx.ty_rptr(
                self.fmtsp,
                self.ecx.ty_ident(self.fmtsp, self.ecx.ident_of("str")),
                Some(static_lifetime),
                ast::MutImmutable);
        lets.push(Context::item_static_array(self.ecx,
                                             static_str_name,
                                             piece_ty,
                                             self.str_pieces));

        // Then, build up the static array which will store our precompiled
        // nonstandard placeholders, if there are any.
        let static_args_name = self.ecx.ident_of("__STATIC_FMTARGS");
        if !self.all_pieces_simple {
            let piece_ty = self.ecx.ty_path(self.ecx.path_all(
                    self.fmtsp,
                    true, Context::rtpath(self.ecx, "Argument"),
                    vec![static_lifetime],
                    vec![]
                ), None);
            lets.push(Context::item_static_array(self.ecx,
                                                 static_args_name,
                                                 piece_ty,
                                                 self.pieces));
        }

        // Right now there is a bug such that for the expression:
        //      foo(bar(&1))
        // the lifetime of `1` doesn't outlast the call to `bar`, so it's not
        // valid for the call to `foo`. To work around this all arguments to the
        // format! string are shoved into locals. Furthermore, we shove the address
        // of each variable because we don't want to move out of the arguments
        // passed to this function.
        for (i, e) in self.args.into_iter().enumerate() {
            let arg_ty = match self.arg_types[i].as_ref() {
                Some(ty) => ty,
                None => continue // error already generated
            };

            let name = self.ecx.ident_of(format!("__arg{}", i).as_slice());
            pats.push(self.ecx.pat_ident(e.span, name));
            locals.push(Context::format_arg(self.ecx, e.span, arg_ty,
                                            self.ecx.expr_ident(e.span, name)));
            heads.push(self.ecx.expr_addr_of(e.span, e));
        }
        for name in self.name_ordering.iter() {
            let e = match self.names.pop(name) {
                Some(e) => e,
                None => continue
            };
            let arg_ty = match self.name_types.find(name) {
                Some(ty) => ty,
                None => continue
            };

            let lname = self.ecx.ident_of(format!("__arg{}",
                                                  *name).as_slice());
            pats.push(self.ecx.pat_ident(e.span, lname));
            names[self.name_positions[*name]] =
                Some(Context::format_arg(self.ecx, e.span, arg_ty,
                                         self.ecx.expr_ident(e.span, lname)));
            heads.push(self.ecx.expr_addr_of(e.span, e));
        }

        // Now create a vector containing all the arguments
        let slicename = self.ecx.ident_of("__args_vec");
        {
            let args = names.into_iter().map(|a| a.unwrap());
            let mut args = locals.into_iter().chain(args);
            let args = self.ecx.expr_vec_slice(self.fmtsp, args.collect());
            lets.push(self.ecx.stmt_let(self.fmtsp, false, slicename, args));
        }

        // Now create the fmt::Arguments struct with all our locals we created.
        let pieces = self.ecx.expr_ident(self.fmtsp, static_str_name);
        let args_slice = self.ecx.expr_ident(self.fmtsp, slicename);

        let (fn_name, fn_args) = if self.all_pieces_simple {
            ("new", vec![pieces, args_slice])
        } else {
            let fmt = self.ecx.expr_ident(self.fmtsp, static_args_name);
            ("with_placeholders", vec![pieces, fmt, args_slice])
        };

        let result = self.ecx.expr_call_global(self.fmtsp, vec!(
                self.ecx.ident_of("std"),
                self.ecx.ident_of("fmt"),
                self.ecx.ident_of("Arguments"),
                self.ecx.ident_of(fn_name)), fn_args);

        // We did all the work of making sure that the arguments
        // structure is safe, so we can safely have an unsafe block.
        let result = self.ecx.expr_block(P(ast::Block {
           view_items: Vec::new(),
           stmts: Vec::new(),
           expr: Some(result),
           id: ast::DUMMY_NODE_ID,
           rules: ast::UnsafeBlock(ast::CompilerGenerated),
           span: self.fmtsp,
        }));
        let resname = self.ecx.ident_of("__args");
        lets.push(self.ecx.stmt_let(self.fmtsp, false, resname, result));
        let res = self.ecx.expr_ident(self.fmtsp, resname);
        let result = match invocation {
            Call(e) => {
                let span = e.span;
                self.ecx.expr_call(span, e,
                                   vec!(self.ecx.expr_addr_of(span, res)))
            }
            MethodCall(e, m) => {
                let span = e.span;
                self.ecx.expr_method_call(span, e, m,
                                          vec!(self.ecx.expr_addr_of(span, res)))
            }
        };
        let body = self.ecx.expr_block(self.ecx.block(self.fmtsp, lets,
                                                      Some(result)));

        // Constructs an AST equivalent to:
        //
        //      match (&arg0, &arg1) {
        //          (tmp0, tmp1) => body
        //      }
        //
        // It was:
        //
        //      let tmp0 = &arg0;
        //      let tmp1 = &arg1;
        //      body
        //
        // Because of #11585 the new temporary lifetime rule, the enclosing
        // statements for these temporaries become the let's themselves.
        // If one or more of them are RefCell's, RefCell borrow() will also
        // end there; they don't last long enough for body to use them. The
        // match expression solves the scope problem.
        //
        // Note, it may also very well be transformed to:
        //
        //      match arg0 {
        //          ref tmp0 => {
        //              match arg1 => {
        //                  ref tmp1 => body } } }
        //
        // But the nested match expression is proved to perform not as well
        // as series of let's; the first approach does.
        let pat = self.ecx.pat(self.fmtsp, ast::PatTup(pats));
        let arm = self.ecx.arm(self.fmtsp, vec!(pat), body);
        let head = self.ecx.expr(self.fmtsp, ast::ExprTup(heads));
        self.ecx.expr_match(self.fmtsp, head, vec!(arm))
    }

    fn format_arg(ecx: &ExtCtxt, sp: Span,
                  ty: &ArgumentType, arg: P<ast::Expr>)
                  -> P<ast::Expr> {
        let trait_ = match *ty {
            Known(ref tyname) => {
                match tyname.as_slice() {
                    ""  => "Show",
                    "b" => "Bool",
                    "c" => "Char",
                    "d" | "i" => "Signed",
                    "e" => "LowerExp",
                    "E" => "UpperExp",
                    "f" => "Float",
                    "o" => "Octal",
                    "p" => "Pointer",
                    "s" => "String",
                    "t" => "Binary",
                    "u" => "Unsigned",
                    "x" => "LowerHex",
                    "X" => "UpperHex",
                    _ => {
                        ecx.span_err(sp,
                                     format!("unknown format trait `{}`",
                                             *tyname).as_slice());
                        "Dummy"
                    }
                }
            }
            Unsigned => {
                return ecx.expr_call_global(sp, vec![
                        ecx.ident_of("std"),
                        ecx.ident_of("fmt"),
                        ecx.ident_of("argumentuint")], vec![arg])
            }
        };

        let format_fn = ecx.path_global(sp, vec![
                ecx.ident_of("std"),
                ecx.ident_of("fmt"),
                ecx.ident_of(trait_),
                ecx.ident_of("fmt")]);
        ecx.expr_call_global(sp, vec![
                ecx.ident_of("std"),
                ecx.ident_of("fmt"),
                ecx.ident_of("argument")], vec![ecx.expr_path(format_fn), arg])
    }
}

pub fn expand_format_args<'cx>(ecx: &'cx mut ExtCtxt, sp: Span,
                               tts: &[ast::TokenTree])
                               -> Box<base::MacResult+'cx> {

    match parse_args(ecx, sp, false, tts) {
        (invocation, Some((efmt, args, order, names))) => {
            MacExpr::new(expand_preparsed_format_args(ecx, sp, invocation, efmt,
                                                      args, order, names))
        }
        (_, None) => MacExpr::new(ecx.expr_uint(sp, 2))
    }
}

pub fn expand_format_args_method<'cx>(ecx: &'cx mut ExtCtxt, sp: Span,
                                      tts: &[ast::TokenTree]) -> Box<base::MacResult+'cx> {

    match parse_args(ecx, sp, true, tts) {
        (invocation, Some((efmt, args, order, names))) => {
            MacExpr::new(expand_preparsed_format_args(ecx, sp, invocation, efmt,
                                                      args, order, names))
        }
        (_, None) => MacExpr::new(ecx.expr_uint(sp, 2))
    }
}

/// Take the various parts of `format_args!(extra, efmt, args...,
/// name=names...)` and construct the appropriate formatting
/// expression.
pub fn expand_preparsed_format_args(ecx: &mut ExtCtxt, sp: Span,
                                    invocation: Invocation,
                                    efmt: P<ast::Expr>,
                                    args: Vec<P<ast::Expr>>,
                                    name_ordering: Vec<string::String>,
                                    names: HashMap<string::String, P<ast::Expr>>)
                                    -> P<ast::Expr> {
    let arg_types = Vec::from_fn(args.len(), |_| None);
    let mut cx = Context {
        ecx: ecx,
        args: args,
        arg_types: arg_types,
        names: names,
        name_positions: HashMap::new(),
        name_types: HashMap::new(),
        name_ordering: name_ordering,
        nest_level: 0,
        next_arg: 0,
        literal: string::String::new(),
        pieces: Vec::new(),
        str_pieces: Vec::new(),
        all_pieces_simple: true,
        method_statics: Vec::new(),
        fmtsp: sp,
    };
    cx.fmtsp = efmt.span;
    let fmt = match expr_to_string(cx.ecx,
                                efmt,
                                "format argument must be a string literal.") {
        Some((fmt, _)) => fmt,
        None => return DummyResult::raw_expr(sp)
    };

    let mut parser = parse::Parser::new(fmt.get());
    loop {
        match parser.next() {
            Some(piece) => {
                if parser.errors.len() > 0 { break }
                cx.verify_piece(&piece);
                match cx.trans_piece(&piece) {
                    Some(piece) => {
                        let s = cx.trans_literal_string();
                        cx.str_pieces.push(s);
                        cx.pieces.push(piece);
                    }
                    None => {}
                }
            }
            None => break
        }
    }
    match parser.errors.remove(0) {
        Some(error) => {
            cx.ecx.span_err(cx.fmtsp,
                            format!("invalid format string: {}",
                                    error).as_slice());
            return DummyResult::raw_expr(sp);
        }
        None => {}
    }
    if !cx.literal.is_empty() {
        let s = cx.trans_literal_string();
        cx.str_pieces.push(s);
    }

    // Make sure that all arguments were used and all arguments have types.
    for (i, ty) in cx.arg_types.iter().enumerate() {
        if ty.is_none() {
            cx.ecx.span_err(cx.args[i].span, "argument never used");
        }
    }
    for (name, e) in cx.names.iter() {
        if !cx.name_types.contains_key(name) {
            cx.ecx.span_err(e.span, "named argument never used");
        }
    }

    cx.to_expr(invocation)
}
