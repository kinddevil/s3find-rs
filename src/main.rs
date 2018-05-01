#[macro_use]
extern crate structopt;

#[macro_use]
extern crate failure;

extern crate chrono;
extern crate clap;
extern crate futures;
extern crate glob;
extern crate regex;
extern crate rusoto_core;
extern crate rusoto_s3;

use structopt::StructOpt;
use structopt::clap::AppSettings;
use std::process::*;
use std::process::Command;
use std::str::FromStr;
use std::path::Path;
use std::fs;
use std::fs::File;
use std::io::Write;

use failure::Error;
use futures::stream::Stream;
use futures::Future;

use glob::Pattern;
use glob::MatchOptions;
use regex::Regex;

use rusoto_core::request::*;
use rusoto_core::Region;
use rusoto_core::ProvideAwsCredentials;

use rusoto_s3::*;
use chrono::prelude::*;

#[derive(Fail, Debug)]
enum FindError {
    #[fail(display = "Invalid s3 path")]
    S3Parse,
    #[fail(display = "Invalid size parameter")]
    SizeParse,
    #[fail(display = "Invalid mtime parameter")]
    TimeParse,
    #[fail(display = "Invalid command line value")]
    CommandlineParse,
    #[fail(display = "Invalid path value")]
    ParentPathParse,
}

type Result<T> = ::std::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq)]
struct S3path {
    bucket: String,
    prefix: Option<String>,
}

impl S3path {
    fn new(path: &str) -> Result<S3path> {
        let s3_vec: Vec<&str> = path.split("/").collect();
        let bucket = s3_vec.get(2).unwrap_or(&"");
        let prefix = s3_vec.get(3).map(|x| x.to_owned());

        let is_validated =
            (s3_vec.get(0) == Some(&"s3:")) && (s3_vec.get(1) == Some(&"")) && (bucket != &"");

        if is_validated {
            Ok(S3path {
                bucket: bucket.to_string(),
                prefix: prefix.map(|x| (*x).to_string()),
            })
        } else {
            Err(FindError::S3Parse.into())
        }
    }
}

impl FromStr for S3path {
    type Err = Error;

    fn from_str(s: &str) -> Result<S3path> {
        S3path::new(s)
    }
}

#[derive(Debug, Clone, PartialEq)]
enum FindSize {
    Equal(i64),
    Bigger(i64),
    Lower(i64),
}

impl FromStr for FindSize {
    type Err = Error;

    fn from_str(s: &str) -> Result<FindSize> {
        let re = Regex::new(r"([+-]?)(\d*)([kMGTP]?)$")?;
        let m = re.captures(s).unwrap();

        let sign = m.get(1).unwrap().as_str().chars().next();
        let number: i64 = m.get(2).unwrap().as_str().parse()?;
        let metric = m.get(3).unwrap().as_str().chars().next();

        let bytes = match metric {
            None => number,
            Some('k') => number * 1024,
            Some('M') => number * 1024_i64.pow(2),
            Some('G') => number * 1024_i64.pow(3),
            Some('T') => number * 1024_i64.pow(4),
            Some('P') => number * 1024_i64.pow(5),
            Some(_) => return Err(FindError::SizeParse.into()),
        };

        match sign {
            Some('+') => Ok(FindSize::Bigger(bytes)),
            Some('-') => Ok(FindSize::Lower(bytes)),
            None => Ok(FindSize::Equal(bytes)),
            Some(_) => Err(FindError::SizeParse.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum FindTime {
    Upper(i64),
    Lower(i64),
}

impl FromStr for FindTime {
    type Err = Error;

    fn from_str(s: &str) -> Result<FindTime> {
        let re = Regex::new(r"([+-]?)(\d*)([smhdw]?)$")?;
        let m = re.captures(s).unwrap();

        let sign = m.get(1).unwrap().as_str().chars().next();
        let number: i64 = m.get(2).unwrap().as_str().parse()?;
        let metric = m.get(3).unwrap().as_str().chars().next();

        let seconds = match metric {
            None => number,
            Some('s') => number,
            Some('m') => number * 60,
            Some('h') => number * 3600,
            Some('d') => number * 3600 * 24,
            Some('w') => number * 3600 * 24 * 7,
            Some(_) => return Err(FindError::TimeParse.into()),
        };

        match sign {
            Some('+') => Ok(FindTime::Upper(seconds)),
            Some('-') => Ok(FindTime::Lower(seconds)),
            None => Ok(FindTime::Upper(seconds)),
            Some(_) => Err(FindError::TimeParse.into()),
        }
    }
}

type NameGlob = Pattern;

#[derive(Debug, Clone, PartialEq)]
struct InameGlob(Pattern);

impl FromStr for InameGlob {
    type Err = Error;

    fn from_str(s: &str) -> Result<InameGlob> {
        let pattern = Pattern::from_str(s)?;
        Ok(InameGlob(pattern))
    }
}

trait Filter {
    fn filter(&self, object: &Object) -> bool;
}

impl Filter for NameGlob {
    fn filter(&self, object: &Object) -> bool {
        let object_key = object.key.as_ref().map(|x| x.as_ref()).unwrap_or_default();
        self.matches(&object_key)
    }
}

impl Filter for InameGlob {
    fn filter(&self, object: &Object) -> bool {
        let object_key = object.key.as_ref().map(|x| x.as_ref()).unwrap_or_default();
        self.0.matches_with(
            &object_key,
            &MatchOptions {
                case_sensitive: false,
                require_literal_separator: false,
                require_literal_leading_dot: false,
            },
        )
    }
}

impl Filter for Regex {
    fn filter(&self, object: &Object) -> bool {
        let object_key = object.key.as_ref().map(|x| x.as_ref()).unwrap_or_default();
        self.is_match(&object_key)
    }
}

impl Filter for FindSize {
    fn filter(&self, object: &Object) -> bool {
        let object_size = object.size.as_ref().unwrap_or(&0);
        match *self {
            FindSize::Bigger(size) => *object_size >= size,
            FindSize::Lower(size) => *object_size <= size,
            FindSize::Equal(size) => *object_size == size,
        }
    }
}

impl Filter for FindTime {
    fn filter(&self, object: &Object) -> bool {
        let object_time = object.last_modified.as_ref().unwrap();
        let last_modified_time = object_time.parse::<DateTime<Utc>>().unwrap().timestamp();
        let now = Utc::now().timestamp();

        match *self {
            FindTime::Upper(seconds) => (now - last_modified_time) >= seconds,
            FindTime::Lower(seconds) => (now - last_modified_time) <= seconds,
        }
    }
}

#[derive(StructOpt, Debug, Clone)]
#[structopt(name = "s3find", about = "walk a s3 path hierarchy",
            raw(global_settings = "&[AppSettings::ColoredHelp, AppSettings::NeedsLongHelp, AppSettings::NeedsSubcommandHelp]"))]
pub struct FindOpt {
    #[structopt(name = "path", raw(index = r#"1"#))]
    path: S3path,
    #[structopt(name = "aws_access_key", long = "aws-access-key",
                help = "AWS key to access to S3, unrequired",
                raw(requires_all = r#"&["aws_secret_key"]"#))]
    aws_access_key: Option<String>,
    #[structopt(name = "aws_secret_key", long = "aws-secret-key",
                help = "AWS secret key to access to S3, unrequired",
                raw(requires_all = r#"&["aws_access_key"]"#))]
    aws_secret_key: Option<String>,
    #[structopt(name = "aws_region", long = "aws-region",
                help = "AWS region to access to S3, unrequired")]
    aws_region: Option<Region>,
    #[structopt(name = "npatern", long = "name", help = "match by glob shell pattern",
                raw(number_of_values = "1"))]
    name: Vec<NameGlob>,
    #[structopt(name = "ipatern", long = "iname",
                help = "match by glob shell pattern, case insensitive",
                raw(number_of_values = "1"))]
    iname: Vec<InameGlob>,
    #[structopt(name = "rpatern", long = "regex",
                help = "match by regex pattern, case insensitive", raw(number_of_values = "1"))]
    regex: Vec<Regex>,
    #[structopt(name = "time", long = "mtime",
                help = "the difference between the file last modification time",
                raw(number_of_values = "1", allow_hyphen_values = "true"))]
    mtime: Vec<FindTime>,
    #[structopt(name = "bytes_size", long = "size", help = "file size",
                raw(number_of_values = "1", allow_hyphen_values = "true"))]
    size: Vec<FindSize>,
    #[structopt(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(StructOpt, Debug, PartialEq, Clone)]
pub enum Cmd {
    #[structopt(name = "-exec", help = "exec any shell comand with every key")]
    Exec {
        #[structopt(name = "utility")]
        utility: String,
    },
    #[structopt(name = "-print", help = "extended print with detail information")]
    Print,
    #[structopt(name = "-delete", help = "delete filtered keys")]
    Delete,
    #[structopt(name = "-download", help = "download filtered keys")]
    Download {
        #[structopt(name = "destination")]
        destination: String,
    },
    #[structopt(name = "-ls", help = "list of filtered keys")]
    Ls,
}

impl FindOpt {
    fn command<P, D>(&self, client: &S3Client<P, D>, bucket: &str, list: Vec<&Object>) -> Result<()>
    where
        P: ProvideAwsCredentials + 'static,
        D: DispatchSignedRequest + 'static,
    {
        match self.cmd {
            Some(Cmd::Print) => {
                let _nlist: Vec<_> = list.iter().map(|x| advanced_print(bucket, x)).collect();
            }
            Some(Cmd::Ls) => {
                let _nlist: Vec<_> = list.iter().map(|x| fprint(bucket, x)).collect();
            }
            Some(Cmd::Exec { utility: ref p }) => {
                let _nlist: Vec<_> = list.iter()
                    .map(|x| {
                        let key = x.key.as_ref().unwrap();
                        let path = format!("s3://{}/{}", bucket, key);
                        exec(&p, &path)
                    })
                    .collect();
            }
            Some(Cmd::Delete) => s3_delete(client, bucket, list)?,
            Some(Cmd::Download { destination: ref d }) => s3_download(client, bucket, list, d)?,
            None => {
                let _nlist: Vec<_> = list.iter().map(|x| fprint(bucket, x)).collect();
            }
        }
        Ok(())
    }
}

struct FilterList(Vec<Box<Filter>>);

impl FilterList {
    fn new(opts: &FindOpt) -> FilterList {
        let mut list: Vec<Box<Filter>> = Vec::new();

        for name in opts.name.iter() {
            list.push(Box::new(name.clone()));
        }

        for iname in opts.iname.iter() {
            list.push(Box::new(iname.clone()));
        }

        for regex in opts.regex.iter() {
            list.push(Box::new(regex.clone()));
        }

        for size in opts.size.iter() {
            list.push(Box::new(size.clone()));
        }

        for mtime in opts.mtime.iter() {
            list.push(Box::new(mtime.clone()));
        }

        FilterList(list)
    }

    fn filters(&self, object: &Object) -> bool {
        for item in self.0.iter() {
            if !item.filter(object) {
                return false;
            }
        }

        true
    }
}

#[derive(Debug, PartialEq, Clone)]
struct ExecStatus {
    status: ExitStatus,
    output: String,
}

fn fprint(bucket: &str, item: &Object) {
    println!(
        "s3://{}/{}",
        bucket,
        item.key.as_ref().unwrap_or(&"".to_string())
    );
}

fn advanced_print(bucket: &str, item: &Object) {
    println!(
        "{} {:?} {} {} s3://{}/{} {}",
        item.e_tag.as_ref().unwrap_or(&"NoEtag".to_string()),
        item.owner.as_ref().map(|x| x.display_name.as_ref()),
        item.size.as_ref().unwrap_or(&0),
        item.last_modified.as_ref().unwrap_or(&"NoTime".to_string()),
        bucket,
        item.key.as_ref().unwrap_or(&"".to_string()),
        item.storage_class
            .as_ref()
            .unwrap_or(&"NoStorage".to_string()),
    );
}

fn exec(command: &str, key: &str) -> Result<ExecStatus> {
    let scommand = command.replace("{}", key);

    let mut command_args = scommand.split(" ");
    let command_name = command_args.next().ok_or(FindError::CommandlineParse)?;

    let mut rcommand = Command::new(command_name);
    for arg in command_args {
        rcommand.arg(arg);
    }

    let output = rcommand.output()?;
    let output_str = String::from_utf8_lossy(&output.stdout).to_string();
    print!("{}", &output_str);

    Ok(ExecStatus {
        status: output.status,
        output: output_str,
    })
}

fn s3_delete<P, D>(client: &S3Client<P, D>, bucket: &str, list: Vec<&Object>) -> Result<()>
where
    P: ProvideAwsCredentials + 'static,
    D: DispatchSignedRequest + 'static,
{
    let key_list: Vec<_> = list.iter()
        .map(|x| ObjectIdentifier {
            key: x.key.as_ref().unwrap().to_string(),
            version_id: None,
        })
        .collect();

    let request = DeleteObjectsRequest {
        bucket: bucket.to_string(),
        delete: Delete {
            objects: key_list,
            quiet: None,
        },
        mfa: None,
        request_payer: None,
    };

    let result = client.delete_objects(&request).sync()?;

    if let Some(deleted_list) = result.deleted {
        for object in deleted_list {
            println!(
                "deleted: s3://{}/{}",
                bucket,
                object.key.as_ref().unwrap_or(&"".to_string())
            );
        }
    }

    Ok(())
}

fn s3_download<P, D>(
    client: &S3Client<P, D>,
    bucket: &str,
    list: Vec<&Object>,
    target: &str,
) -> Result<()>
where
    P: ProvideAwsCredentials + 'static,
    D: DispatchSignedRequest + 'static,
{
    for object in list.iter() {
        let key = object.key.as_ref().unwrap();
        let request = GetObjectRequest {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
            ..Default::default()
        };

        let file_path = format!("{}/{}", target, key);
        let dir_path = Path::new(&file_path)
            .parent()
            .ok_or(FindError::ParentPathParse)?;

        fs::create_dir_all(&dir_path)?;

        let result = client.get_object(&request).sync()?;

        let mut output = File::create(&file_path)?;
        let mut input = result.body.unwrap().concat2().wait().unwrap();

        output.write(&input)?;

        println!("downloaded: s3://{}/{} to {}", bucket, &key, &file_path);
    }

    Ok(())
}

fn real_main() -> Result<()> {
    let status = FindOpt::from_args();
    let s3path = status.path.clone();

    let filter = FilterList::new(&status);

    let region = status.aws_region.clone().unwrap_or(Region::default());
    let client = S3Client::simple(region);

    let mut request = ListObjectsV2Request {
        bucket: s3path.bucket.clone(),
        continuation_token: None,
        delimiter: None,
        encoding_type: None,
        fetch_owner: None,
        max_keys: Some(10000),
        prefix: s3path.prefix,
        request_payer: None,
        start_after: None,
    };

    loop {
        let output = client.list_objects_v2(&request).sync()?;
        match output.contents {
            Some(klist) => {
                let flist: Vec<_> = klist.iter().filter(|x| filter.filters(x)).collect();
                status.command(&client, &s3path.bucket, flist)?;

                match output.next_continuation_token {
                    Some(token) => request.continuation_token = Some(token),
                    None => break,
                }
            }
            None => {
                println!("No keys!");
                break;
            }
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = real_main() {
        eprintln!("Error - {:#?}", error);
        ::std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use rusoto_s3::*;
    use exec;
    use advanced_print;
    use S3path;
    use FindSize;
    use FindTime;

    #[test]
    fn s3path_corect() {
        let url = "s3://testbucket/";
        let path = S3path::new(url).unwrap();
        assert_eq!(path.bucket, "testbucket", "This should be 'testbucket'");
        assert_eq!(
            path.prefix,
            Some("".to_string()),
            "This should be empty path"
        );
    }

    #[test]
    fn s3path_correct_full() {
        let url = "s3://testbucket/path";
        let path = S3path::new(url).unwrap();
        assert_eq!(path.bucket, "testbucket", "This should be 'testbucket'");
        assert_eq!(
            path.prefix,
            Some("path".to_string()),
            "This should be 'path'"
        );
    }

    #[test]
    fn s3path_correct_short() {
        let url = "s3://testbucket";
        let path = S3path::new(url).unwrap();
        assert_eq!(path.bucket, "testbucket", "This should be 'testbucket'");
        assert_eq!(path.prefix, None, "This should be None");
    }

    #[test]
    fn s3path_only_bucket() {
        let url = "testbucket";
        let path = S3path::new(url);
        assert!(
            path.is_err(),
            "This s3 url should not be validated posivitely"
        );
    }

    #[test]
    fn s3path_without_bucket() {
        let url = "s3://";
        let path = S3path::new(url);
        assert!(
            path.is_err(),
            "This s3 url should not be validated posivitely"
        );
    }

    #[test]
    fn s3path_without_2_slash() {
        let url = "s3:/testbucket";
        let path = S3path::new(url);
        assert!(
            path.is_err(),
            "This s3 url should not be validated posivitely"
        );
    }

    #[test]
    fn exec_true() {
        let exec_status = exec("true", "").unwrap();
        assert!(exec_status.status.success(), "Exit code of true is 0");
    }

    #[test]
    fn exec_false() {
        let exec_status = exec("false", "");
        assert!(
            !exec_status.unwrap().status.success(),
            "Exit code of false is 1"
        );
    }

    #[test]
    fn exec_echo_multiple() {
        let exec_status = exec("echo Hello world1", "").unwrap();

        assert!(exec_status.status.success(), "Exit code of echo is 0");
        assert_eq!(
            exec_status.output, "Hello world1\n",
            "Output of echo is 'Hello world1\n'"
        );
    }

    #[test]
    #[should_panic]
    fn exec_incorrect_command() {
        let exec_status = exec("jbrwuDxPy4ck", "");
        assert!(
            !exec_status.unwrap().status.success(),
            "Exit code should not be success"
        );
    }

    #[test]
    fn exec_echo_interpolation() {
        let exec_status = exec("echo Hello {}", "world2").unwrap();

        assert!(exec_status.status.success(), "Exit code of echo is 0");
        assert_eq!(
            exec_status.output, "Hello world2\n",
            "String should interpolated and printed"
        );
    }

    #[test]
    fn advanced_print_test() {
        let object = Object {
            e_tag: Some("9d48114aa7c18f9d68aa20086dbb7756".to_string()),
            key: Some("somepath/otherpath".to_string()),
            last_modified: Some("2017-07-19T19:04:17.000Z".to_string()),
            owner: None,
            size: Some(4997288),
            storage_class: Some("STANDARD".to_string()),
        };
        advanced_print("bucket", &object);
    }

    #[test]
    fn size_corect() {
        let size_str = "1111";
        let size = size_str.parse::<FindSize>();

        assert_eq!(size.ok(), Some(FindSize::Equal(1111)), "should be equal");
    }

    #[test]
    fn size_corect_k() {
        let size_str = "1111k";
        let size = size_str.parse::<FindSize>();

        assert_eq!(
            size.ok(),
            Some(FindSize::Equal(1111 * 1024)),
            "should be equal"
        );
    }

    #[test]
    fn size_corect_positive() {
        let size_str = "+1111";
        let size = size_str.parse::<FindSize>();

        assert_eq!(size.ok(), Some(FindSize::Bigger(1111)), "should be upper");
    }

    #[test]
    fn size_corect_positive_k() {
        let size_str = "+1111k";
        let size = size_str.parse::<FindSize>();

        assert_eq!(
            size.ok(),
            Some(FindSize::Bigger(1111 * 1024)),
            "should be upper"
        );
    }

    #[test]
    fn size_corect_negative() {
        let size_str = "-1111";
        let size = size_str.parse::<FindSize>();

        assert_eq!(size.ok(), Some(FindSize::Lower(1111)), "should be lower");
    }

    #[test]
    fn size_corect_negative_k() {
        let size_str = "-1111k";
        let size = size_str.parse::<FindSize>();

        assert_eq!(
            size.ok(),
            Some(FindSize::Lower(1111 * 1024)),
            "should be lower"
        );
    }

    #[test]
    fn size_incorect_negative() {
        let size_str = "-";
        let size = size_str.parse::<FindSize>();

        assert!(size.is_err(), "Should be error");
    }

    #[test]
    fn size_incorect_negative_s() {
        let size_str = "-123w";
        let size = size_str.parse::<FindSize>();

        assert!(size.is_err(), "Should be error");
    }

    #[test]
    fn time_corect() {
        let time_str = "1111";
        let time = time_str.parse::<FindTime>();

        assert!(time.is_ok(), "Should be ok");
        assert_eq!(time.ok(), Some(FindTime::Upper(1111)), "Should be upper");
    }

    #[test]
    fn time_corect_m() {
        let time_str = "10m";
        let time = time_str.parse::<FindTime>();

        assert!(time.is_ok(), "Should be ok");
        assert_eq!(time.ok(), Some(FindTime::Upper(600)), "Should be upper");
    }

    #[test]
    fn time_corect_positive() {
        let time_str = "+1111";
        let time = time_str.parse::<FindTime>();

        assert!(time.is_ok(), "Should be ok");
        assert_eq!(time.ok(), Some(FindTime::Upper(1111)), "Should be upper");
    }

    #[test]
    fn time_corect_positive_m() {
        let time_str = "+10m";
        let time = time_str.parse::<FindTime>();

        assert!(time.is_ok(), "Should be ok");
        assert_eq!(time.ok(), Some(FindTime::Upper(600)), "Should be upper");
    }

    #[test]
    fn time_corect_negative_m() {
        let time_str = "-10m";
        let time = time_str.parse::<FindTime>();

        assert!(time.is_ok(), "Should be ok");
        assert_eq!(time.ok(), Some(FindTime::Lower(600)), "Should be upper");
    }

    #[test]
    fn time_corect_negative() {
        let time_str = "-1111";
        let time = time_str.parse::<FindTime>();

        assert!(time.is_ok(), "Should be ok");
        assert_eq!(time.ok(), Some(FindTime::Lower(1111)), "Should be lower");
    }

    #[test]
    fn time_incorect_negative() {
        let time_str = "-";
        let time = time_str.parse::<FindTime>();
        assert!(time.is_err(), "Should be error");
    }

    #[test]
    fn time_incorect_negative_t() {
        let time_str = "-10t";
        let time = time_str.parse::<FindTime>();
        assert!(time.is_err(), "Should be error");
    }

    #[test]
    fn time_incorect_positive() {
        let time_str = "+";
        let time = time_str.parse::<FindTime>();
        assert!(time.is_err(), "Should be error");
    }

    #[test]
    fn time_incorect_positive_t() {
        let time_str = "+10t";
        let time = time_str.parse::<FindTime>();
        assert!(time.is_err(), "Should be error");
    }
}
