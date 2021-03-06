// Copyright 2019 Fabian Schuiki
// Copyright 2019 Florian Zaruba

// SPDX-License-Identifier: Apache-2.0
#![recursion_limit = "256"]

#[macro_use]
extern crate log;

use anyhow::{Context as _, Error, Result};
use clap::{App, Arg};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::{Arc, Mutex};
use sv_parser::preprocess;
use sv_parser::Error as SvParserError;
use sv_parser::{parse_sv_str, unwrap_node, Define, DefineText, Locate, RefNode, SyntaxTree};

pub mod doc;
mod printer;

/// Struct containing information about
/// what should be pickled and how.
#[derive(Debug)]
struct Pickle<'a> {
    /// Optional name prefix.
    prefix: Option<&'a str>,
    /// Optional name suffix.
    suffix: Option<&'a str>,
    /// Declarations which are excluded from re-naming.
    exclude: HashSet<&'a str>,
    /// Table containing thing that should be re-named.
    rename_table: HashMap<String, String>,
    /// Locations of text which should be replaced.
    replace_table: Vec<(usize, usize, String)>,
}

impl<'a> Pickle<'a> {
    /// Register a declaration such as a package or module.
    fn register_declaration(&mut self, syntax_tree: &SyntaxTree, id: RefNode) {
        let (module_name, loc) = get_identifier(syntax_tree, id);
        if self.exclude.contains(module_name.as_str()) {
            return;
        }
        let mut new_name = module_name.clone();
        if let Some(prefix) = self.prefix {
            new_name = format!("{}{}", prefix, new_name);
        }
        if let Some(suffix) = self.suffix {
            new_name = format!("{}{}", new_name, suffix);
        }
        debug!("Declaration `{}`: {:?}", module_name, loc);
        self.rename_table.insert(module_name, new_name);
    }

    /// Register a usage of the identifier.
    fn register_usage(&mut self, syntax_tree: &SyntaxTree, id: RefNode) {
        let (inst_name, loc) = get_identifier(&syntax_tree, id);
        let new_name = match self.rename_table.get(&inst_name) {
            Some(x) => x,
            None => return,
        };
        debug!("Usage `{}`: {:?}", inst_name, loc);
        self.replace_table
            .push((loc.offset, loc.len, new_name.clone()));
    }
}

fn main() -> Result<()> {
    let matches = App::new(env!("CARGO_PKG_NAME"))
        .version(clap::crate_version!())
        .author(clap::crate_authors!())
        .about(clap::crate_description!())
        .arg(
            Arg::with_name("inc")
                .short("I")
                .value_name("DIR")
                .help("Add a search path for SystemVerilog includes")
                .multiple(true)
                .takes_value(true)
                .number_of_values(1),
        )
        .arg(
            Arg::with_name("exclude")
                .short("e")
                .long("exclude")
                .value_name("MODULE")
                .help("Add modules which should not be renamed")
                .multiple(true)
                .takes_value(true)
                .number_of_values(1),
        )
        .arg(
            Arg::with_name("v")
                .short("v")
                .multiple(true)
                .help("Sets the level of verbosity"),
        )
        .arg(
            Arg::with_name("prefix")
                .short("p")
                .long("prefix")
                .value_name("PREFIX")
                .help("Prepend a name to all global names")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("def")
                .short("D")
                .value_name("DEFINE")
                .help("Define a preprocesor macro")
                .multiple(true)
                .takes_value(true)
                .number_of_values(1),
        )
        .arg(
            Arg::with_name("suffix")
                .short("s")
                .long("suffix")
                .value_name("SUFFIX")
                .help("Append a name to all global names")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("preproc")
                .short("E")
                .help("Write preprocessed input files to stdout"),
        )
        .arg(
            Arg::with_name("file_list")
                .short("f")
                .value_name("LIST")
                .help("Gather files from a manifest")
                .multiple(true)
                .takes_value(true)
                .number_of_values(1),
        )
        .arg(
            Arg::with_name("minimize")
                .long("minimize")
                .help("Minimize the output (also strips comments)"),
        )
        .arg(
            Arg::with_name("strip_comments")
                .long("strip-comments")
                .help("Strip comments from the output"),
        )
        .arg(
            Arg::with_name("INPUT")
                .help("The input files to compile")
                .multiple(true),
        )
        .arg(
            Arg::with_name("docdir")
                .short("d")
                .long("doc")
                .value_name("OUTDIR")
                .help("Generate documentation in a directory")
                .takes_value(true),
        )
        .get_matches();

    // Instantiate a new logger with the verbosity level the user requested.
    simple_logger::init_with_level(match matches.occurrences_of("v") {
        0 => log::Level::Warn,
        1 => log::Level::Info,
        2 => log::Level::Debug,
        3 | _ => log::Level::Trace,
    })
    .unwrap();

    let mut file_list = Vec::new();

    // Handle user defines.
    let defines: HashMap<_, _> = match matches.values_of("def") {
        Some(args) => args
            .map(|x| {
                let mut iter = x.split('=');
                (
                    iter.next().unwrap().to_string(),
                    iter.next().map(String::from),
                )
            })
            .collect(),
        None => HashMap::new(),
    };

    // Prepare a list of include paths.
    let include_dirs: Vec<_> = matches
        .values_of("inc")
        .into_iter()
        .flatten()
        .map(|x| x.to_string())
        .collect();

    for path in matches.values_of("file_list").into_iter().flatten() {
        let file = File::open(path).unwrap();
        let reader = BufReader::new(file);

        // Read the JSON contents of the file as an instance of `User`.
        let mut u: Vec<FileBundle> = serde_json::from_reader(reader).unwrap();
        for fb in &mut u {
            fb.defines.extend(defines.clone());
            fb.include_dirs.extend(include_dirs.clone());
        }
        file_list.extend(u);
    }

    if let Some(file_names) = matches.values_of("INPUT") {
        file_list.push(FileBundle {
            include_dirs,
            defines,
            files: file_names.map(String::from).collect(),
        });
    }

    let mut exclude = HashSet::new();
    exclude.extend(matches.values_of("exclude").into_iter().flatten());

    let mut pickle = Pickle {
        // Collect renaming options.
        prefix: matches.value_of("prefix"),
        suffix: matches.value_of("suffix"),
        exclude,
        // Create a rename table.
        rename_table: HashMap::new(),
        replace_table: Vec::new(),
    };

    // Parse the input files.
    let printer = Arc::new(Mutex::new(printer::Printer::new()));
    let mut syntax_trees = vec![];

    let minimize = matches.is_present("minimize");
    let strip_comments = matches.is_present("strip_comments") | minimize;
    for bundle in file_list {
        let bundle_include_dirs: Vec<_> = bundle.include_dirs.iter().map(Path::new).collect();
        // Convert the preprocessor defines into the appropriate format which is understood by `sv-parser`
        let bundle_defines: HashMap<_, _> = bundle
            .defines
            .iter()
            .map(|(name, value)| {
                // If there is a define text add it.
                let define_text = match value {
                    Some(x) => Some(DefineText::new(String::from(x), None)),
                    None => None,
                };
                (
                    name.clone(),
                    Some(Define::new(name.clone(), vec![], define_text)),
                )
            })
            .collect();

        // For each file in the file bundle preprocess and parse it.
        // Use a neat trick of `collect` here, which allows you to collect a
        // `Result<T>` iterator into a `Result<Vec<T>>`, i.e. bubbling up the
        // error.
        let v: Result<Vec<ParsedFile>> = bundle
            .files
            .par_iter()
            .map(|filename| -> Result<_> {
                info!("{:?}", filename);

                // Preprocess the verilog files.
                let buffer = String::from(
                    preprocess(
                        filename,
                        &bundle_defines,
                        &bundle_include_dirs,
                        strip_comments,
                        false,
                    )
                    .with_context(|| format!("Failed to preprocess `{}`", filename))?
                    .0
                    .text(),
                );

                // Optionally minimize the pre-processed string.
                let mut buffer = if minimize {
                    let mut ret_buffer = String::new();
                    for s in buffer.replace("\n", " ").split_ascii_whitespace() {
                        ret_buffer.push_str(s);
                        ret_buffer.push(' ');
                    }
                    ret_buffer
                } else {
                    buffer
                };
                // print!("{}", buffer);

                // Make sure that each file ends with a newline.
                if !buffer.ends_with("\n") {
                    buffer.push('\n');
                }
                let syntax_tee = parse_sv_str(
                    buffer.as_str(),
                    filename,
                    &HashMap::new(),
                    &Vec::<String>::new(),
                    false,
                )
                .or_else(|err| -> Result<_> {
                    let mut printer = &mut *printer.lock().unwrap();
                    print_parse_error(&mut printer, &err, false)?;
                    Err(Error::new(err))
                })?
                .0;

                Ok(ParsedFile {
                    path: filename.clone(),
                    source: buffer,
                    ast: syntax_tee,
                })
            })
            .collect();
        syntax_trees.extend(v?);
    }

    // Just preprocess.
    if matches.is_present("preproc") {
        for pf in syntax_trees {
            eprintln!("{}:", pf.path);
            println!("{:}", pf.source);
        }
        return Ok(());
    }

    info!("Finished reading {} source files.", syntax_trees.len());

    // Emit documentation if requested.
    if let Some(dir) = matches.value_of("docdir") {
        info!("Generating documentation in `{}`", dir);
        let doc = doc::Doc::new(&syntax_trees);
        let mut html = doc::Renderer::new(Path::new(dir));
        html.render(&doc)?;
        return Ok(());
    }

    // Gather information for pickling.
    for pf in &syntax_trees {
        for node in &pf.ast {
            trace!("{:?}", node);
            match node {
                // Module declarations.
                RefNode::ModuleDeclarationAnsi(x) => {
                    // unwrap_node! gets the nearest ModuleIdentifier from x
                    let id = unwrap_node!(x, SimpleIdentifier).unwrap();
                    pickle.register_declaration(&pf.ast, id);
                }
                RefNode::ModuleDeclarationNonansi(x) => {
                    let id = unwrap_node!(x, SimpleIdentifier).unwrap();
                    pickle.register_declaration(&pf.ast, id);
                }
                // Interface Declaration.
                RefNode::InterfaceDeclaration(x) => {
                    let id = unwrap_node!(x, SimpleIdentifier).unwrap();
                    pickle.register_declaration(&pf.ast, id);
                }
                // Package declarations.
                RefNode::PackageDeclaration(x) => {
                    let id = unwrap_node!(x, SimpleIdentifier).unwrap();
                    pickle.register_declaration(&pf.ast, id);
                }
                _ => (),
            }
        }
    }

    // Emit the pickled source files.
    for pf in &syntax_trees {
        // For each file, start with a clean replacement table.
        pickle.replace_table.clear();
        // Iterate again and check for usage
        for node in &pf.ast {
            match node {
                // Instantiations, end-labels.
                RefNode::ModuleIdentifier(x) => {
                    let id = unwrap_node!(x, SimpleIdentifier).unwrap();
                    pickle.register_usage(&pf.ast, id);
                }
                // Interface identifier.
                RefNode::InterfaceIdentifier(x) => {
                    let id = unwrap_node!(x, SimpleIdentifier).unwrap();
                    pickle.register_usage(&pf.ast, id);
                }
                // Package Qualifier (i.e., explicit package constants).
                RefNode::ClassScope(x) => {
                    let id = unwrap_node!(x, SimpleIdentifier).unwrap();
                    pickle.register_usage(&pf.ast, id);
                }
                // Package Import.
                RefNode::PackageIdentifier(x) => {
                    let id = unwrap_node!(x, SimpleIdentifier).unwrap();
                    pickle.register_usage(&pf.ast, id);
                }
                _ => (),
            }
        }
        // Replace according to `replace_table`.
        // Apply the replacements.
        debug!("{:?}", pickle.replace_table);
        let mut pos = 0;
        for (offset, len, repl) in pickle.replace_table.iter() {
            trace!("Replacing: {},{}, {}", offset, len, repl);
            print!("{}", &pf.source[pos..*offset]);
            print!("{}", repl);
            pos = offset + len;
        }
        print!("{}", &pf.source[pos..]);
    }

    Ok(())
}

fn get_identifier(st: &SyntaxTree, node: RefNode) -> (String, Locate) {
    // unwrap_node! can take multiple types
    match unwrap_node!(node, SimpleIdentifier, EscapedIdentifier) {
        Some(RefNode::SimpleIdentifier(x)) => {
            // Original string can be got by SyntaxTree::get_str(self, locate: &Locate)
            (String::from(st.get_str(&x.nodes.0).unwrap()), x.nodes.0)
        }
        Some(RefNode::EscapedIdentifier(x)) => {
            (String::from(st.get_str(&x.nodes.0).unwrap()), x.nodes.0)
        }
        _ => panic!("No identifier found."),
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct FileBundle {
    include_dirs: Vec<String>,
    defines: HashMap<String, Option<String>>,
    files: Vec<String>,
}

/// A parsed input file.
pub struct ParsedFile {
    /// The path to the file.
    pub path: String,
    /// The contents of the file.
    pub source: String,
    /// The parsed AST of the file.
    pub ast: SyntaxTree,
}

#[cfg_attr(tarpaulin, skip)]
fn print_parse_error(
    printer: &mut printer::Printer,
    error: &SvParserError,
    single: bool,
) -> Result<()> {
    match error {
        SvParserError::Parse(Some((path, pos))) => {
            printer.print_parse_error(path, *pos, single)?;
        }
        SvParserError::Include { source: x } => {
            if let SvParserError::File { path: x, .. } = x.as_ref() {
                printer.print_error(&format!("failed to include '{}'", x.display()))?;
            }
        }
        SvParserError::DefineArgNotFound(x) => {
            printer.print_error(&format!("define argument '{}' is not found", x))?;
        }
        SvParserError::DefineNotFound(x) => {
            printer.print_error(&format!("define '{}' is not found", x))?;
        }
        x => {
            printer.print_error(&format!("{}", x))?;
        }
    }

    Ok(())
}
