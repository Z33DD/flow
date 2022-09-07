//! DO NOT EDIT THIS FILE!
//! This file is generated by regenerate-tests.sh based on the official
//! test cases in the submodule.

mod validator_test_utils;
use validator_test_utils::run_draft09_format_test;

// NOTE: no true (i.e non-punycode) internationalized hostnames are supported
// If provided, they will fail validation, so that we don't run into a
// situation in the future where previously-passing schemas start to fail.
// If we need this in the future, let's revisit (jshearer)
// #[test]
// fn test_d09_format_idn_email() {
//     run_draft09_format_test("idn-email.json");
// }

// #[test]
// fn test_d09_format_idn_hostname() {
//     run_draft09_format_test("idn-hostname.json");
// }

#[test]
fn test_d09_format_date_time() {
    run_draft09_format_test("date-time.json");
}

#[test]
fn test_d09_format_date() {
    run_draft09_format_test("date.json");
}

#[test]
fn test_d09_format_duration() {
    run_draft09_format_test("duration.json");
}

#[test]
fn test_d09_format_email() {
    run_draft09_format_test("email.json");
}

#[test]
fn test_d09_format_hostname() {
    run_draft09_format_test("hostname.json");
}

#[test]
fn test_d09_format_ipv4() {
    run_draft09_format_test("ipv4.json");
}

#[test]
fn test_d09_format_ipv6() {
    run_draft09_format_test("ipv6.json");
}

#[test]
fn test_d09_format_iri_reference() {
    run_draft09_format_test("iri-reference.json");
}

#[test]
fn test_d09_format_iri() {
    run_draft09_format_test("iri.json");
}

#[test]
fn test_d09_format_json_pointer() {
    run_draft09_format_test("json-pointer.json");
}

#[test]
fn test_d09_format_regex() {
    run_draft09_format_test("regex.json");
}

#[test]
fn test_d09_format_relative_json_pointer() {
    run_draft09_format_test("relative-json-pointer.json");
}

#[test]
fn test_d09_format_time() {
    run_draft09_format_test("time.json");
}

#[test]
fn test_d09_format_uri_reference() {
    run_draft09_format_test("uri-reference.json");
}

#[test]
fn test_d09_format_uri_template() {
    run_draft09_format_test("uri-template.json");
}

#[test]
fn test_d09_format_uri() {
    run_draft09_format_test("uri.json");
}

#[test]
fn test_d09_format_uuid() {
    run_draft09_format_test("uuid.json");
}
