use std::{str::FromStr, time::Duration};

/// Application configuration
#[derive(clap::Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Config {
    /// The collection of PIDs to monitor.
    #[arg(name = "pid", required = true, num_args = 1..)]
    pub pids: Vec<i32>,
    /// The maximum time to collect statistics.
    #[arg(short, long, value_parser = parse_timeout_duration)]
    pub timeout: Option<Duration>,
    #[arg(
        name = "field",
        short,
        long,
        help = "sum | all_loads | if_greater:value:then[:else]",
        default_values = ["sum", "all_loads"]
    )]
    /// The list of output fields to print with each update.
    pub fields: Vec<Field>,
}

/// Specification of one field of information to print about a collection of PIDs.
#[derive(Clone, Debug)]
pub enum Field {
    /// Print the sum of all process trees' CPU usage.
    Sum,
    /// Print CPU usage of each process tree.
    AllLoads,
    /// Print one string when total CPU usage of all process trees is above a threshold, or a
    /// different string otherwise.
    IfGreater {
        /// The threshold.
        value: f32,
        /// String to be printed when CPU usage above threshold.
        then: String,
        /// String to be printed otherwise.
        otherwise: Option<String>,
    },
}

impl FromStr for Field {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut tokens = value.split(':');
        let field = tokens.next().expect("should produce at least 1 elment");
        match field {
            "" => Err("missing field name")?,
            "sum" => Ok(Field::Sum),
            "all_loads" => Ok(Field::AllLoads),
            "if_greater" => {
                let value: f32 = tokens
                    .next()
                    .ok_or("missing value")?
                    .parse()
                    .map_err(|e| format!("wrong value: {e}"))?;
                let then = tokens.next().ok_or("missing then-clause")?.to_owned();
                let otherwise = tokens.next().map(|t| t.to_owned());
                Ok(Field::IfGreater {
                    value,
                    then,
                    otherwise,
                })
            }
            _ => Err(format!("unknown field {field}"))?,
        }
    }
}

fn parse_timeout_duration(arg: &str) -> Result<std::time::Duration, std::num::ParseIntError> {
    let seconds = arg.parse()?;
    Ok(std::time::Duration::from_secs(seconds))
}
