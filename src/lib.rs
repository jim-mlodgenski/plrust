// Copyright (c) 2020, ZomboDB, LLC
use pgx::*;

mod gucs;
mod plrust;

pg_module_magic!();

#[pg_guard]
fn _PG_init() {
    gucs::init();
    plrust::init();
}

/// `pgx` doesn't know how to declare a CREATE FUNCTION statement for a function
/// whose only argument is a `pg_sys::FunctionCallInfo`, so we gotta do that ourselves.
///
/// ```sql
/// CREATE OR REPLACE FUNCTION plrustu_call_handler() RETURNS language_handler
///     LANGUAGE c AS 'MODULE_PATHNAME', 'plrustu_call_handler_wrapper';
/// ```
#[pg_extern]
unsafe fn plrustu_call_handler(fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum {
    let fn_oid = fcinfo.as_ref().unwrap().flinfo.as_ref().unwrap().fn_oid;
    let func = plrust::lookup_function(fn_oid);

    func(fcinfo)
}

#[pg_extern]
unsafe fn plrustu_validator(fn_oid: pg_sys::Oid, fcinfo: pg_sys::FunctionCallInfo) {
    let fcinfo = PgBox::from_pg(fcinfo);
    let flinfo = PgBox::from_pg(fcinfo.flinfo);
    if !pg_sys::CheckFunctionValidatorAccess(flinfo.fn_oid, pg_getarg(fcinfo.as_ptr(), 0).unwrap())
    {
        return;
    }

    plrust::unload_function(fn_oid);
    // NOTE:  We purposely ignore the `check_function_bodies` GUC for compilation as we need to
    // compile the function when it's created to avoid locking during function execution
    let (_, output) =
        plrust::compile_function(fn_oid).unwrap_or_else(|e| panic!("compilation failed\n{}", e));

    // however, we'll use it to decide if we should go ahead and dynamically load our function
    if pg_sys::check_function_bodies {
        // it's on, so lets go ahead and load our function
        // plrust::lookup_function(fn_oid);
    }

    // if the compilation had warnings we'll display them
    if output.contains("warning: ") {
        pgx::warning!("\n{}", output);
    }
}

#[pg_extern]
fn recompile_function(
    fn_oid: pg_sys::Oid,
) -> (
    name!(library_path, Option<String>),
    name!(cargo_output, String),
) {
    unsafe {
        plrust::unload_function(fn_oid);
    }
    match plrust::compile_function(fn_oid) {
        Ok((work_dir, output)) => (Some(work_dir.display().to_string()), output),
        Err(e) => (None, e),
    }
}

extension_sql!(
    r#"
CREATE LANGUAGE plrustu
    HANDLER plrust.plrustu_call_handler
    VALIDATOR plrust.plrustu_validator;
    
COMMENT ON LANGUAGE plrustu IS 'PL/rustu untrusted procedural language';
"#
);

#[cfg(any(test, feature = "pg_test"))]
mod tests {
    use super::*;

    #[pg_test]
    #[search_path(@extschema@)]
    fn test_basic() {
        let definition = r#"
            CREATE OR REPLACE FUNCTION sum_array(a BIGINT[]) RETURNS BIGINT
                IMMUTABLE STRICT
                LANGUAGE PLRUSTU AS
            $$
                Some(a.into_iter().map(|v| v.unwrap_or_default()).sum())
            $$;
        "#;
        Spi::run(definition);

        let retval = Spi::get_one_with_args(
            r#"
            SELECT sum_array($1);
        "#,
            vec![(
                PgBuiltInOids::INT4ARRAYOID.oid(),
                vec![1, 2, 3].into_datum(),
            )],
        );
        assert_eq!(retval, Some(6));
    }

    #[pg_test]
    #[cfg(not(feature = "sandboxed"))]
    #[search_path(@extschema@)]
    fn test_deps() {
        let definition = r#"
            CREATE OR REPLACE FUNCTION zalgo(input TEXT) RETURNS TEXT
                IMMUTABLE STRICT
                LANGUAGE PLRUSTU AS
            $$
            [dependencies]
                zalgo = "0.2.0"
            [code]
                use zalgo::{Generator, GeneratorArgs, ZalgoSize};

                let mut generator = Generator::new();
                let mut out = String::new();
                let args = GeneratorArgs::new(true, false, false, ZalgoSize::Maxi);
                let result = generator.gen(input, &mut out, &args);

                Some(out)
            $$;
        "#;
        Spi::run(definition);

        let retval: Option<String> = Spi::get_one_with_args(
            r#"
            SELECT zalgo($1);
        "#,
            vec![(PgBuiltInOids::TEXTOID.oid(), "Nami".into_datum())],
        );
        assert!(retval.is_some());
    }
}

#[cfg(test)]
pub mod pg_test {
    use once_cell::sync::Lazy;
    use pgx_utils::pg_config::Pgx;
    use tempdir::TempDir;

    static WORK_DIR: Lazy<String> = Lazy::new(|| {
        let work_dir = TempDir::new("plrust-tests").expect("Couldn't create tempdir");
        format!("plrust.work_dir='{}'", work_dir.path().display())
    });
    static PG_CONFIG: Lazy<String> = Lazy::new(|| {
        let pgx_config = Pgx::from_config().unwrap();
        let version = format!("pg{}", pgx_pg_sys::get_pg_major_version_num());
        let pg_config = pgx_config.get(&version).unwrap();
        let path = pg_config.path().unwrap();
        format!("plrust.pg_config='{}'", path.as_path().display())
    });

    pub fn setup(_options: Vec<&str>) {
        // perform one-off initialization when the pg_test framework starts
    }

    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![&*WORK_DIR, &*PG_CONFIG]
    }
}
