
#![feature(test)]
// Initial Bench:
// test test_path_clean      ... bench:       3,233 ns/iter (+/- 121)
// test test_path_clean_long ... bench:   5,980,001 ns/iter (+/- 20,708)

extern crate httprouter;
extern crate test;

use httprouter::path::*;
use test::Bencher;

// path, result
fn clean_tests() -> Vec<(&'static str, &'static str)> {
    vec![
        // Already clean
        ("/", "/"),
        ("/abc", "/abc"),
        ("/a/b/c", "/a/b/c"),
        ("/abc/", "/abc/"),
        ("/a/b/c/", "/a/b/c/"),
        // missing root
        ("", "/"),
        ("a/", "/a/"),
        ("abc", "/abc"),
        ("abc/def", "/abc/def"),
        ("a/b/c", "/a/b/c"),
        // Remove doubled slash
        ("//", "/"),
        ("/abc//", "/abc/"),
        ("/abc/def//", "/abc/def/"),
        ("/a/b/c//", "/a/b/c/"),
        ("/abc//def//ghi", "/abc/def/ghi"),
        ("//abc", "/abc"),
        ("///abc", "/abc"),
        ("//abc//", "/abc/"),
        // Remove . elements
        (".", "/"),
        ("./", "/"),
        ("/abc/./def", "/abc/def"),
        ("/./abc/def", "/abc/def"),
        ("/abc/.", "/abc/"),
        // Remove .. elements
        ("..", "/"),
        ("../", "/"),
        ("../../", "/"),
        ("../..", "/"),
        ("../../abc", "/abc"),
        ("/abc/def/ghi/../jkl", "/abc/def/jkl"),
        ("/abc/def/../ghi/../jkl", "/abc/jkl"),
        ("/abc/def/..", "/abc"),
        ("/abc/def/../..", "/"),
        ("/abc/def/../../..", "/"),
        ("/abc/def/../../..", "/"),
        ("/abc/def/../../../ghi/jkl/../../../mno", "/mno"),
        // Combinations
        ("abc/./../def", "/def"),
        ("abc//./../def", "/def"),
        ("abc/../../././../def", "/def"),
    ]
}

#[bench]
fn test_path_clean(b: &mut Bencher) {
    let tests = clean_tests();

    b.iter(|| {
        for test in &tests {
            test::black_box(clean(test.0));
            test::black_box(clean(test.1));
        }
    });
}

#[bench]
fn test_path_clean_long(b: &mut Bencher) {
    let mut test_paths: Vec<(String, String)> = Vec::new();
    for i in 1..1234 {
        let ss = "a".repeat(i);

        let correct_path = format!("{}{}", "/", ss);
        test_paths.push((correct_path.clone(), correct_path.clone()));
        test_paths.push((ss.clone(), correct_path.clone()));
        test_paths.push((format!("{}{}", "//", ss), correct_path.clone()));
        test_paths.push((format!("{}{}{}", "//", ss, "/b/.."), correct_path.clone()));
    }

    b.iter(|| {
        for test in &test_paths {
            test::black_box(clean(&test.0));
            test::black_box(clean(&test.1));
        }
    });
}
