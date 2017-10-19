extern crate distributary;
extern crate rand;

#[macro_use]
extern crate clap;
#[macro_use]
extern crate slog;

use distributary::{Blender, DataType, Recipe, ReuseConfigType};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::{thread, time};

#[macro_use]
mod populate;

use populate::{Populate, NANOS_PER_SEC};

pub struct Backend {
    recipe: Option<Recipe>,
    log: slog::Logger,
    g: Blender,
}

impl Backend {
    pub fn new(partial: bool, shard: bool, reuse: &str) -> Backend {
        let mut g = Blender::new();
        let log = distributary::logger_pls();
        let blender_log = log.clone();
        g.log_with(blender_log);

        let mut recipe = Recipe::blank(Some(log.clone()));

        if !partial {
            g.disable_partial();
        }

        if !shard {
            g.disable_sharding();
        }

        match reuse.as_ref() {
            "finkelstein" => recipe.enable_reuse(ReuseConfigType::Finkelstein),
            "full" => recipe.enable_reuse(ReuseConfigType::Full),
            "noreuse" => recipe.enable_reuse(ReuseConfigType::NoReuse),
            "relaxed" => recipe.enable_reuse(ReuseConfigType::Relaxed),
            _ => panic!("reuse configuration not supported"),
        }

        Backend {
            recipe: Some(recipe),
            log: log,
            g: g,
        }
    }

    pub fn recipe(&self) -> Recipe {
        match self.recipe {
            Some(ref r) => r.clone(),
            None => panic!("recipe doesn't exist"),
        }
    }

    fn login(&mut self, user_context: HashMap<String, DataType>) -> Result<(), String> {
        {
            let mut mig = self.g.add_universe(user_context.clone());
            let mut recipe = self.recipe.take().unwrap();
            recipe.next();
            match recipe.create_universe(&mut mig) {
                Ok(ar) => {
                    info!(self.log, "{} expressions added", ar.expressions_added);
                    info!(self.log, "{} expressions removed", ar.expressions_removed);
                }
                Err(e) => panic!("failed to activate recipe: {}", e),
            };
            mig.commit();
            self.recipe = Some(recipe);
        }

        self.write_to_user_context(user_context);
        Ok(())
    }

    fn write_to_user_context(&self, uc: HashMap<String, DataType>) {
        let name = &format!("UserContext_{}", uc.get("id").unwrap());
        let r: Vec<DataType> = uc.values().cloned().collect();
        let mut mutator = self
            .g
            .get_mutator(self.recipe().node_addr_for(name).unwrap());

        mutator.put(r).unwrap();
    }

    fn migrate(
        &mut self,
        schema_file: &str,
        query_file: Option<&str>,
        policy_file: Option<&str>,
    ) -> Result<(), String> {
        use std::fs::File;
        use std::io::Read;

        // Start migration
        let mut mig = self.g.start_migration();

        // Read schema file
        let mut sf = File::open(schema_file).unwrap();
        let mut s = String::new();
        sf.read_to_string(&mut s).unwrap();

        let mut rs = s.clone();
        s.clear();

        match query_file {
            None => (),
            Some(qf) => {
                let mut qf = File::open(qf).unwrap();
                qf.read_to_string(&mut s).unwrap();
                rs.push_str("\n");
                rs.push_str(&s);
            }
        }

        let mut p = String::new();
        let pstr: Option<&str> = match policy_file {
            None => None,
            Some(pf) => {
                let mut pf = File::open(pf).unwrap();
                pf.read_to_string(&mut p).unwrap();
                Some(&p)
            }
        };

        let new_recipe = Recipe::from_str_with_policy(&rs, pstr, Some(self.log.clone()))?;
        let cur_recipe = self.recipe.take().unwrap();
        let updated_recipe = match cur_recipe.replace(new_recipe) {
            Ok(mut recipe) => {
                match recipe.activate(&mut mig, false) {
                    Ok(ar) => {
                        info!(self.log, "{} expressions added", ar.expressions_added);
                        info!(self.log, "{} expressions removed", ar.expressions_removed);
                    }
                    Err(e) => return Err(format!("failed to activate recipe: {}", e)),
                };
                recipe
            }
            Err(e) => return Err(format!("failed to replace recipe: {}", e)),
        };

        mig.commit();
        self.recipe = Some(updated_recipe);
        Ok(())
    }
}

fn make_user(id: i32) -> HashMap<String, DataType> {
    let mut user = HashMap::new();
    user.insert(String::from("id"), id.into());

    user
}

fn main() {
    use clap::{App, Arg};
    let args = App::new("piazza")
        .version("0.1")
        .about("Benchmarks Piazza-like application with security policies.")
        .arg(
            Arg::with_name("schema")
                .short("s")
                .required(true)
                .default_value("benchmarks/piazza/schema.sql")
                .help("Schema file for Piazza application"),
        )
        .arg(
            Arg::with_name("queries")
                .short("q")
                .required(true)
                .default_value("benchmarks/piazza/queries.sql")
                .help("Query file for Piazza application"),
        )
        .arg(
            Arg::with_name("policies")
                .long("policies")
                .required(true)
                .default_value("benchmarks/piazza/policies.json")
                .help("Security policies file for Piazza application"),
        )
        .arg(
            Arg::with_name("graph")
                .short("g")
                .default_value("pgraph.gv")
                .help("File to dump application's soup graph, if set"),
        )
        .arg(
            Arg::with_name("reuse")
                .long("reuse")
                .default_value("full")
                .possible_values(&["noreuse", "finkelstein", "relaxed", "full"])
                .help("Query reuse algorithm"),
        )
        .arg(
            Arg::with_name("shard")
                .long("shard")
                .help("Enable sharding"),
        )
        .arg(
            Arg::with_name("partial")
                .long("partial")
                .help("Enable partial materialization"),
        )
        .arg(
            Arg::with_name("populate")
                .long("populate")
                .help("Populate app with randomly generated data"),
        )
        .arg(
            Arg::with_name("nusers")
                .short("u")
                .default_value("1000")
                .help("Number of users in the db"),
        )
        .arg(
            Arg::with_name("nlogged")
                .short("l")
                .default_value("1000")
                .help("Number of logged users. Must be less or equal than the number of users in the db")
            )
        .arg(
            Arg::with_name("nclasses")
                .short("c")
                .default_value("100")
                .help("Number of classes in the db"),
        )
        .arg(
            Arg::with_name("nposts")
                .short("p")
                .default_value("100000")
                .help("Number of posts in the db"),
        )
        .arg(
            Arg::with_name("private")
                .long("private")
                .default_value("0.1")
                .help("Percentage of private posts"),
        )
        .get_matches();


    println!("Starting benchmark...");

    // Read arguments
    let sloc = args.value_of("schema").unwrap();
    let qloc = args.value_of("queries").unwrap();
    let ploc = args.value_of("policies").unwrap();
    let gloc = args.value_of("graph");
    let partial = args.is_present("partial");
    let shard = args.is_present("shard");
    let reuse = args.value_of("reuse").unwrap();
    let populate = args.is_present("populate");
    let nusers = value_t_or_exit!(args, "nusers", i32);
    let nlogged = value_t_or_exit!(args, "nlogged", i32);
    let nclasses = value_t_or_exit!(args, "nclasses", i32);
    let nposts = value_t_or_exit!(args, "nposts", i32);
    let private = value_t_or_exit!(args, "private", f32);

    assert!(nlogged <= nusers, "nusers must be greater than nlogged");

    // Initiliaze backend application with some queries and policies
    println!("Initiliazing database schema...");
    let mut backend = Backend::new(partial, shard, reuse);
    backend.migrate(sloc, Some(qloc), Some(ploc)).unwrap();

    // Login a user
    println!("Login in users...");
    for i in 0..nlogged {
        let start = time::Instant::now();
        backend.login(make_user(i)).is_ok();
        let dur = dur_to_fsec!(start.elapsed());
        println!(
            "Migration {} took {:.2}s!",
            i,
            dur,
        );
    }

    let mut p = Populate::new(nposts, nusers, nclasses, private);
    if populate {
        println!("Populating tables...");
        p.populate_tables(&backend);
    }

    println!("Finished writing! Sleeping for 2 seconds...");
    thread::sleep(time::Duration::from_millis(2000));


    println!("Done with benchmark.");

    if gloc.is_some() {
        let graph_fname = gloc.unwrap();
        let mut gf = File::create(graph_fname).unwrap();
        assert!(write!(gf, "{:x}", backend.g).is_ok());
    }
}
