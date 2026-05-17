use fakeset::{graph::build_dag, load_all_datasets};
use petgraph::visit::Topo;
use std::path::PathBuf;

#[test]
fn test_basic_valid_dag() {
    let paths = vec![PathBuf::from("tests/fixtures/basic")];
    let datasets = load_all_datasets(&paths).expect("should load all datasets");
    assert_eq!(datasets.len(), 4);
    build_dag(&datasets).expect("basic include structure should form a valid DAG");
}

#[test]
fn test_list_content_include_orders_provider_before_consumer() {
    // events.yaml has a rich list field whose content includes people.yaml.
    // The DAG must add a data-dependency edge that makes people a predecessor of events,
    // so the topo sort visits people before events.
    let paths = vec![PathBuf::from("tests/fixtures/execute/rich_list")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let dag = build_dag(&datasets).expect("should build dag");

    let mut topo = Topo::new(&dag.graph);
    let mut order: Vec<String> = Vec::new();
    while let Some(idx) = topo.next(&dag.graph) {
        let path = &dag.graph[idx];
        if let Some(ds) = datasets.get(path) {
            order.push(ds.name.clone());
        }
    }

    let people_pos = order.iter().position(|n| n == "people").expect("'people' not in topo order");
    let events_pos = order.iter().position(|n| n == "events").expect("'events' not in topo order");
    assert!(
        people_pos < events_pos,
        "people (list-content provider) must precede events (consumer) in topo order; got: {order:?}"
    );
}

#[test]
fn test_cyclic_includes_detected() {
    let paths = vec![PathBuf::from("tests/fixtures/cyclic")];
    let datasets = load_all_datasets(&paths).expect("should load all datasets");
    let err = build_dag(&datasets).expect_err("cyclic includes should be rejected");
    assert!(
        err.to_string().contains("circular include"),
        "unexpected error message: {err}"
    );
}