import braintrust


def test_logger_spans_round_trip(project_name, api):
    logger = braintrust.init_logger(project=project_name)
    with logger.start_span(name="root") as root:
        root.log(input={"q": "hi"}, output="hello", metadata={"k": "v"})
        with root.start_span(name="child") as child:
            child.log(output="nested")
    logger.flush()

    events = api.fetch("project_logs", api.project_id(project_name))
    by_name = {e["span_attributes"]["name"]: e for e in events}
    assert by_name["root"]["input"] == {"q": "hi"}
    assert by_name["root"]["output"] == "hello"
    assert by_name["root"]["metadata"] == {"k": "v"}
    assert by_name["child"]["span_parents"] == [by_name["root"]["span_id"]]
    assert by_name["child"]["root_span_id"] == by_name["root"]["root_span_id"]


def test_logger_log_with_scores_and_feedback(project_name, api):
    logger = braintrust.init_logger(project=project_name)
    row_id = logger.log(input="a", output="b", scores={"quality": 0.5})
    logger.log_feedback(id=row_id, scores={"quality": 1.0}, comment="better")
    logger.flush()

    events = api.fetch("project_logs", api.project_id(project_name))
    row = next(e for e in events if e["id"] == row_id)
    assert row["scores"]["quality"] == 1.0


def test_experiment_log_fetch_and_summarize(project_name):
    exp = braintrust.init(project=project_name, experiment="baseline")
    exp.log(input=1, output=2, expected=2, scores={"exact": 1.0})
    exp.log(input=2, output=3, expected=4, scores={"exact": 0.0})
    summary = exp.summarize()
    assert summary.project_name == project_name
    assert summary.experiment_name == "baseline"
    assert summary.scores["exact"].score == 0.5

    readonly = braintrust.init(project=project_name, experiment="baseline", open=True)
    rows = list(readonly.fetch())
    assert sorted(r["input"] for r in rows) == [1, 2]


def test_experiment_compares_against_base(project_name):
    base = braintrust.init(project=project_name, experiment="base")
    base.log(input=1, output=1, scores={"exact": 0.0})
    base.summarize()

    exp = braintrust.init(project=project_name, experiment="next", base_experiment="base")
    exp.log(input=1, output=1, scores={"exact": 1.0})
    summary = exp.summarize()
    assert summary.comparison_experiment_name == "base"
    assert summary.scores["exact"].diff == 1.0
    assert summary.scores["exact"].improvements == 1


def test_dataset_insert_update_delete_iterate(project_name):
    ds = braintrust.init_dataset(project=project_name, name="golden")
    keep = ds.insert(input={"x": 1}, expected=2, tags=["t"])
    drop = ds.insert(input={"x": 9}, expected=0)
    ds.update(id=keep, metadata={"reviewed": True})
    ds.delete(drop)
    ds.flush()

    rows = list(braintrust.init_dataset(project=project_name, name="golden"))
    assert len(rows) == 1
    assert rows[0]["id"] == keep
    assert rows[0]["input"] == {"x": 1}
    assert rows[0]["expected"] == 2
    assert rows[0]["metadata"] == {"reviewed": True}

    summary = ds.summarize()
    assert summary.data_summary.total_records == 1


def test_eval_with_dataset_end_to_end(project_name):
    ds = braintrust.init_dataset(project=project_name, name="math")
    for x in range(5):
        ds.insert(input=x, expected=x * 2)
    ds.flush()

    def exact(input, output, expected):
        return 1.0 if output == expected else 0.0

    result = braintrust.Eval(
        project_name,
        data=braintrust.init_dataset(project=project_name, name="math"),
        task=lambda x: x * 2 if x < 4 else -1,
        scores=[exact],
        experiment_name="doubling",
    )
    assert result.summary.experiment_name == "doubling"
    assert result.summary.scores["exact"].score == 0.8
    assert len(result.results) == 5


def test_load_prompt_latest_and_pinned_version(project_name, api):
    api.request("POST", "/v1/project", {"name": project_name})
    pid = api.project_id(project_name)

    def save(greeting):
        return api.request(
            "PUT",
            "/v1/prompt",
            {
                "project_id": pid,
                "name": "Greeter",
                "slug": "greeter",
                "prompt_data": {
                    "options": {"model": "gpt-4o-mini"},
                    "prompt": {"type": "chat", "messages": [{"role": "user", "content": greeting + " {{name}}"}]},
                },
            },
        )

    v1 = save("Hello")
    save("Howdy")

    latest = braintrust.load_prompt(project=project_name, slug="greeter", no_trace=True)
    assert latest.build(name="Ada")["messages"] == [{"role": "user", "content": "Howdy Ada"}]
    assert latest.build(name="Ada")["model"] == "gpt-4o-mini"

    pinned = braintrust.load_prompt(project=project_name, slug="greeter", version=v1["_xact_id"], no_trace=True)
    assert pinned.build(name="Ada")["messages"][0]["content"] == "Hello Ada"
