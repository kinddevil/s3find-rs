extern crate rusoto_s3;

use rusoto_s3::{Delete, DeleteObjectsRequest, GetObjectRequest, GetObjectTaggingRequest, Object,
                ObjectIdentifier, PutObjectTaggingRequest, S3, S3Client, Tagging};
use std::process::Command;
use std::process::ExitStatus;

use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::Path;

use rusoto_core::request::*;
use rusoto_core::ProvideAwsCredentials;

use futures::stream::Stream;
use futures::Future;

use failure::err_msg;
use indicatif::{ProgressBar, ProgressStyle};

use types::*;

pub fn fprint(bucket: &str, item: &Object) {
    println!(
        "s3://{}/{}",
        bucket,
        item.key.as_ref().unwrap_or(&"".to_string())
    );
}

pub fn advanced_print(bucket: &str, item: &Object) {
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

#[derive(Debug, PartialEq, Clone)]
pub struct ExecStatus {
    pub status: ExitStatus,
    pub runcommand: String,
}

pub fn exec(command: &str, key: &str) -> Result<ExecStatus> {
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
        runcommand: scommand.clone(),
    })
}

pub fn s3_delete<P, D>(client: &S3Client<P, D>, bucket: &str, list: Vec<&Object>) -> Result<()>
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

pub fn s3_download<P, D>(
    client: &S3Client<P, D>,
    bucket: &str,
    list: Vec<&Object>,
    target: &str,
    force: &bool,
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

        let size = (*object.size.as_ref().unwrap()) as u64;
        let file_path = Path::new(target).join(key);
        let dir_path = file_path.parent().ok_or(FindError::ParentPathParse)?;

        let mut count: u64 = 0;
        let pb = ProgressBar::new(size);
        pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")
        .progress_chars("#>-"));
        println!(
            "downloading: s3://{}/{} => {}",
            bucket,
            &key,
            file_path.to_str().ok_or(err_msg("cannot parse filename"))?
        );

        if file_path.exists() && !force {
            return Err(err_msg("file is already present"));
        }

        let result = client.get_object(&request).sync()?;

        let mut stream = result
            .body
            .ok_or(err_msg("cannot fetch body from s3 response"))?;

        fs::create_dir_all(&dir_path)?;
        let mut output = File::create(&file_path)?;

        let _r = stream
            .for_each(|buf| {
                output.write(&buf)?;
                count = count + (buf.len() as u64);
                pb.set_position(count);
                Ok(())
            })
            .wait();
    }

    Ok(())
}

pub fn s3_set_tags<P, D>(
    client: &S3Client<P, D>,
    bucket: &str,
    list: Vec<&Object>,
    tags: Tagging,
) -> Result<()>
where
    P: ProvideAwsCredentials + 'static,
    D: DispatchSignedRequest + 'static,
{
    for object in list.iter() {
        let key = object.key.as_ref().unwrap();

        let request = PutObjectTaggingRequest {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
            tagging: tags.clone(),
            ..Default::default()
        };

        client.put_object_tagging(&request).sync()?;

        println!("tags are set for: s3://{}/{}", bucket, &key);
    }

    Ok(())
}

pub fn s3_list_tags<P, D>(client: &S3Client<P, D>, bucket: &str, list: Vec<&Object>) -> Result<()>
where
    P: ProvideAwsCredentials + 'static,
    D: DispatchSignedRequest + 'static,
{
    for object in list.iter() {
        let key = object.key.as_ref().unwrap();

        let request = GetObjectTaggingRequest {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
            ..Default::default()
        };

        let tag_output = client.get_object_tagging(&request).sync()?;

        let tags: String = tag_output
            .tag_set
            .into_iter()
            .map(|x| format!("{}:{}", x.key, x.value))
            .collect::<Vec<String>>()
            .join(",");

        println!(
            "s3://{}/{} {}",
            bucket,
            object.key.as_ref().unwrap_or(&"".to_string()),
            tags,
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use functions::advanced_print;
    use functions::exec;
    use rusoto_s3::*;

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
            exec_status.runcommand, "echo Hello world1",
            "Output of echo is 'Hello world1'"
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
            exec_status.runcommand, "echo Hello world2",
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

}
