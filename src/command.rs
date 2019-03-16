use failure::Error;
use humansize::{file_size_opts as options, FileSize};
use rusoto_core::request::HttpClient;
use rusoto_core::Region;
use rusoto_s3::{ListObjectsV2Request, Object, S3Client, Tag, Tagging};
use std::default::Default;
use std::fmt;

use crate::arg::*;
use crate::credential::*;
use crate::filter::Filter;
use crate::function::*;

pub struct FilterList(pub Vec<Box<dyn Filter>>);

impl FilterList {
    pub fn test_match(&self, object: &Object) -> bool {
        for item in &self.0 {
            if !item.filter(object) {
                return false;
            }
        }

        true
    }
}

pub struct FindCommand {
    pub client: S3Client,
    pub region: Region,
    pub path: S3path,
    pub filters: FilterList,
    pub limit: Option<usize>,
    pub page_size: i64,
    pub summarize: bool,
    pub command: Option<Cmd>,
}

impl FindCommand {
    #![allow(unreachable_patterns)]
    pub fn exec(&self, list: &[&Object], acc: Option<FindStat>) -> Result<Option<FindStat>, Error> {
        let status = match acc {
            Some(stat) => Some(stat.add(list)),
            None => None,
        };

        match (*self).command {
            Some(Cmd::Print) => {
                let _nlist: Vec<_> = list
                    .iter()
                    .map(|x| advanced_print(&self.path.bucket, x))
                    .collect();
            }
            Some(Cmd::Ls) => {
                let _nlist: Vec<_> = list.iter().map(|x| fprint(&self.path.bucket, x)).collect();
            }
            Some(Cmd::Exec { utility: ref p }) => {
                let _nlist: Vec<_> = list
                    .iter()
                    .map(|x| {
                        let key = x.key.as_ref().map(String::as_str).unwrap_or("");
                        let path = format!("s3://{}/{}", &self.path.bucket, key);
                        exec(&p, &path)
                    })
                    .collect();
            }
            Some(Cmd::Delete) => s3_delete(&self.client, &self.path.bucket, list)?,
            Some(Cmd::Download {
                destination: ref d,
                force: ref f,
            }) => s3_download(&self.client, &self.path.bucket, &list, d, f.to_owned())?,
            Some(Cmd::Tags { tags: ref t }) => {
                let tags = Tagging {
                    tag_set: t.iter().map(|x| x.clone().into()).collect(),
                };
                s3_set_tags(&self.client, &self.path.bucket, &list, &tags)?
            }
            Some(Cmd::LsTags) => s3_list_tags(&self.client, &self.path.bucket, list)?,
            Some(Cmd::Public) => {
                s3_set_public(&self.client, &self.path.bucket, list, &self.region)?
            }
            Some(Cmd::Copy {
                destination: ref d,
                flat: f,
            }) => s3_copy(
                &self.client,
                &self.path.bucket,
                &list,
                &d.bucket,
                &d.clone().prefix.unwrap_or_default(),
                f,
                false,
            )?,
            Some(Cmd::Move {
                destination: ref d,
                flat: f,
            }) => s3_copy(
                &self.client,
                &self.path.bucket,
                &list,
                &d.bucket,
                &d.clone().prefix.unwrap_or_default(),
                f,
                true,
            )?,
            Some(Cmd::Nothing) => {}
            Some(_) => println!("Not implemented"),
            None => {
                let _nlist: Vec<_> = list.iter().map(|x| fprint(&self.path.bucket, x)).collect();
            }
        }
        Ok(status)
    }

    pub fn list_request(&self) -> ListObjectsV2Request {
        ListObjectsV2Request {
            bucket: self.path.bucket.clone(),
            continuation_token: None,
            delimiter: None,
            encoding_type: None,
            fetch_owner: None,
            max_keys: Some(self.page_size),
            prefix: self.path.prefix.clone(),
            request_payer: None,
            start_after: None,
        }
    }

    pub fn stats(&self) -> Option<FindStat> {
        if self.summarize {
            Some(FindStat::default())
        } else {
            None
        }
    }
}

impl From<FindOpt> for FindCommand {
    fn from(opts: FindOpt) -> FindCommand {
        let region = opts.aws_region.clone();
        let provider =
            CombinedProvider::new(opts.aws_access_key.clone(), opts.aws_secret_key.clone());

        let dispatcher = HttpClient::new().unwrap();

        let client = S3Client::new_with(dispatcher, provider, region.clone());

        FindCommand {
            client,
            region,
            filters: opts.clone().into(),
            path: opts.path,
            command: opts.cmd,
            page_size: opts.page_size,
            summarize: opts.summarize,
            limit: opts.limit,
        }
    }
}

impl From<FindOpt> for FilterList {
    fn from(opts: FindOpt) -> FilterList {
        let mut list: Vec<Box<dyn Filter>> = Vec::new();

        for name in &opts.name {
            list.push(Box::new(name.clone()));
        }

        for iname in &opts.iname {
            list.push(Box::new(iname.clone()));
        }

        for regex in &opts.regex {
            list.push(Box::new(regex.clone()));
        }

        for size in &opts.size {
            list.push(Box::new(size.clone()));
        }

        for mtime in &opts.mtime {
            list.push(Box::new(mtime.clone()));
        }

        FilterList(list)
    }
}

impl From<FindTag> for Tag {
    fn from(tag: FindTag) -> Tag {
        Tag {
            key: tag.key,
            value: tag.value,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FindStat {
    pub total_files: usize,
    pub total_space: i64,
    pub max_size: i64,
    pub min_size: i64,
    pub max_key: String,
    pub min_key: String,
    pub average_size: i64,
}

impl FindStat {
    pub fn add(mut self: FindStat, list: &[&Object]) -> FindStat {
        for x in list {
            self.total_files += 1;
            let size = x.size.as_ref().unwrap_or(&0);
            self.total_space += size;

            if self.max_size < *size {
                self.max_size = *size;
                self.max_key = x.key.clone().unwrap_or_default();
            }

            if self.min_size > *size {
                self.min_size = *size;
                self.min_key = x.key.clone().unwrap_or_default();
            }

            self.average_size = self.total_space / (self.total_files as i64);
        }
        self
    }
}

impl Default for FindStat {
    fn default() -> Self {
        FindStat {
            total_files: 0,
            total_space: 0,
            max_size: 0,
            min_size: i64::max_value(),
            max_key: "".to_owned(),
            min_key: "".to_owned(),
            average_size: 0,
        }
    }
}

impl fmt::Display for FindStat {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f)?;
        writeln!(f, "Summary")?;
        writeln!(f, "{:19} {}", "Total files:", &self.total_files)?;
        writeln!(
            f,
            "Total space:        {}",
            &self
                .total_space
                .file_size(options::CONVENTIONAL)
                .map_err(|_| fmt::Error)?
        )?;
        writeln!(f, "{:19} {}", "Largest file:", &self.max_key)?;
        writeln!(
            f,
            "{:19} {}",
            "Largest file size:",
            &self
                .max_size
                .file_size(options::CONVENTIONAL)
                .map_err(|_| fmt::Error)?
        )?;
        writeln!(f, "{:19} {}", "Smallest file:", &self.min_key)?;
        writeln!(
            f,
            "{:19} {}",
            "Smallest file size:",
            &self
                .min_size
                .file_size(options::CONVENTIONAL)
                .map_err(|_| fmt::Error)?
        )?;
        writeln!(
            f,
            "{:19} {}",
            "Average file size:",
            &self
                .average_size
                .file_size(options::CONVENTIONAL)
                .map_err(|_| fmt::Error)?
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;
    use std::str::FromStr;

    #[test]
    fn from_findtag() -> Result<(), Error> {
        let tag: Tag = FindTag {
            key: "tag".to_owned(),
            value: "val".to_owned(),
        }
        .into();

        assert_eq!(
            tag,
            Tag {
                key: "tag".to_owned(),
                value: "val".to_owned(),
            }
        );
        Ok(())
    }

    #[test]
    fn from_findopt_to_findcommand() {
        let find: FindCommand = FindOpt {
            path: S3path {
                bucket: "bucket".to_owned(),
                prefix: Some("prefix".to_owned()),
            },
            aws_access_key: Some("access".to_owned()),
            aws_secret_key: Some("secret".to_owned()),
            aws_region: Region::UsEast1,
            name: vec![NameGlob::from_str("*ref*").unwrap()],
            iname: vec![InameGlob::from_str("Pre*").unwrap()],
            regex: vec![Regex::from_str("^pre").unwrap()],
            mtime: Vec::new(),
            size: vec![FindSize::Lower(1000)],
            limit: None,
            page_size: 1000,
            summarize: false,
            cmd: Some(Cmd::Ls),
        }
        .into();

        assert_eq!(
            find.path,
            S3path {
                bucket: "bucket".to_owned(),
                prefix: Some("prefix".to_owned()),
            }
        );
        assert_eq!(find.region, Region::UsEast1);
        assert_eq!(find.command, Some(Cmd::Ls));

        let object_ok = Object {
            key: Some("pref".to_owned()),
            size: Some(10),
            ..Default::default()
        };
        assert!(find.filters.test_match(&object_ok));

        let object_fail = Object {
            key: Some("Refer".to_owned()),
            size: Some(10),
            ..Default::default()
        };
        assert!(!find.filters.test_match(&object_fail));
    }
}
