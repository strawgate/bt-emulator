package e2e

import (
	"context"
	"sort"
	"strings"
	"testing"

	braintrust "github.com/braintrustdata/braintrust-sdk-go"
	"github.com/braintrustdata/braintrust-sdk-go/api/datasets"
	"github.com/braintrustdata/braintrust-sdk-go/api/functions"
	"github.com/braintrustdata/braintrust-sdk-go/eval"
	"github.com/braintrustdata/braintrust-sdk-go/prompt"
	"go.opentelemetry.io/otel/attribute"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"
)

// newClient returns an SDK client for a fresh project, and its tracer provider.
func newClient(t *testing.T) (*braintrust.Client, *sdktrace.TracerProvider, string) {
	t.Helper()
	project := projectName()
	tp := sdktrace.NewTracerProvider()
	t.Cleanup(func() { _ = tp.Shutdown(context.Background()) })
	client, err := braintrust.New(tp,
		braintrust.WithAPIKey(apiKey),
		braintrust.WithAPIURL(emulatorURL),
		braintrust.WithAppURL(emulatorURL),
		braintrust.WithProject(project),
		braintrust.WithBlockingLogin(true),
		braintrust.WithAutoConvertAIAttachments(false),
	)
	if err != nil {
		t.Fatal(err)
	}
	return client, tp, project
}

func projectID(t *testing.T, name string) string {
	t.Helper()
	objects := rest(t, "GET", "/v1/project?project_name="+name, nil)["objects"].([]any)
	if len(objects) != 1 {
		t.Fatalf("expected one project %s, got %v", name, objects)
	}
	return objects[0].(map[string]any)["id"].(string)
}

func TestTracedSpansReachProjectLogs(t *testing.T) {
	ctx := context.Background()
	client, tp, project := newClient(t)
	ctx, root := client.Tracer("e2e").Start(ctx, "handle-request")
	root.SetAttributes(
		attribute.String("braintrust.input_json", `{"question":"2+2?"}`),
		attribute.String("braintrust.output_json", `"4"`),
		attribute.String("braintrust.metadata", `{"user":"ada"}`),
	)
	_, child := client.Tracer("e2e").Start(ctx, "lookup")
	child.End()
	root.End()
	if err := tp.ForceFlush(ctx); err != nil {
		t.Fatal(err)
	}

	rows := events(t, "project_logs", projectID(t, project))
	if len(rows) != 2 {
		t.Fatalf("expected 2 spans, got %d: %v", len(rows), rows)
	}
	byName := map[string]map[string]any{}
	for _, r := range rows {
		byName[r["span_attributes"].(map[string]any)["name"].(string)] = r
	}
	got := byName["handle-request"]
	if got["output"] != "4" || got["input"].(map[string]any)["question"] != "2+2?" || got["metadata"].(map[string]any)["user"] != "ada" {
		t.Fatalf("root span: %v", got)
	}
	if got["is_root"] != true || byName["lookup"]["is_root"] != false || byName["lookup"]["root_span_id"] != got["root_span_id"] {
		t.Fatalf("span tree: %v", rows)
	}
}

func TestEvalWritesScoredExperiment(t *testing.T) {
	ctx := context.Background()
	client, tp, _ := newClient(t)
	result, err := braintrust.NewEvaluator[string, string](client).Run(ctx, eval.Opts[string, string]{
		Experiment: "upper",
		Dataset: eval.NewDataset([]eval.Case[string, string]{
			{Input: "a", Expected: "A"},
			{Input: "b", Expected: "X"},
		}),
		Task: eval.T(func(ctx context.Context, in string) (string, error) { return strings.ToUpper(in), nil }),
		Scorers: []eval.Scorer[string, string]{eval.NewScorer("exact",
			func(_ context.Context, r eval.TaskResult[string, string]) (eval.Scores, error) {
				if r.Output == r.Expected {
					return eval.S(1), nil
				}
				return eval.S(0), nil
			})},
		Quiet: true,
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := tp.ForceFlush(ctx); err != nil {
		t.Fatal(err)
	}

	var scores []float64
	for _, r := range events(t, "experiment", result.ID()) {
		if s, ok := score(r, "exact"); ok {
			scores = append(scores, s)
		}
	}
	sort.Float64s(scores)
	if len(scores) != 2 || scores[0] != 0 || scores[1] != 1 {
		t.Fatalf("scores: %v", scores)
	}
	summary := rest(t, "GET", "/v1/experiment/"+result.ID()+"/summarize?summarize_scores=true", nil)
	if s := summary["scores"].(map[string]any)["exact"].(map[string]any)["score"]; s != 0.5 {
		t.Fatalf("summary score %v: %v", s, summary)
	}
}

func TestEvalOverAStoredDataset(t *testing.T) {
	ctx := context.Background()
	client, tp, project := newClient(t)
	pid := rest(t, "POST", "/v1/project", map[string]any{"name": project})["id"].(string)
	ds, err := client.API().Datasets().Create(ctx, datasets.CreateParams{ProjectID: pid, Name: "cases"})
	if err != nil {
		t.Fatal(err)
	}
	if err := client.API().Datasets().InsertEvents(ctx, ds.ID, []datasets.Event{
		{Input: "x", Expected: "X"}, {Input: "y", Expected: "Y"}, {Input: "z", Expected: "Z"},
	}); err != nil {
		t.Fatal(err)
	}
	evaluator := braintrust.NewEvaluator[string, string](client)
	dataset, err := evaluator.Datasets().Get(ctx, ds.ID)
	if err != nil {
		t.Fatal(err)
	}
	result, err := evaluator.Run(ctx, eval.Opts[string, string]{
		Experiment: "from-dataset",
		Dataset:    dataset,
		Task:       eval.T(func(ctx context.Context, in string) (string, error) { return strings.ToUpper(in), nil }),
		Scorers: []eval.Scorer[string, string]{eval.NewScorer("exact",
			func(_ context.Context, r eval.TaskResult[string, string]) (eval.Scores, error) { return eval.S(1), nil })},
		Quiet: true,
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := tp.ForceFlush(ctx); err != nil {
		t.Fatal(err)
	}
	roots := 0
	for _, r := range events(t, "experiment", result.ID()) {
		if r["is_root"] == true {
			roots++
		}
	}
	if roots != 3 {
		t.Fatalf("expected 3 eval cases, got %d", roots)
	}
}

func TestLoadPromptAndInvokeFunctions(t *testing.T) {
	ctx := context.Background()
	client, _, project := newClient(t)
	pid := rest(t, "POST", "/v1/project", map[string]any{"name": project})["id"].(string)
	fns := client.API().Functions()
	if _, err := fns.Create(ctx, functions.CreateParams{
		ProjectID: pid, Name: "Greeter", Slug: "greet",
		FunctionData: map[string]any{"type": "prompt"},
		PromptData: map[string]any{
			"options": map[string]any{"model": "stub"},
			"prompt":  map[string]any{"type": "chat", "messages": []any{map[string]any{"role": "user", "content": "Say hello to {{input}}"}}},
		},
	}); err != nil {
		t.Fatal(err)
	}

	p, err := client.LoadPrompt(ctx, prompt.LoadOpts{Slug: "greet"})
	if err != nil {
		t.Fatal(err)
	}
	built, err := p.Build(map[string]any{"input": "Ada"})
	if err != nil {
		t.Fatal(err)
	}
	if built.Model != "stub" || len(built.Messages) != 1 {
		t.Fatalf("built prompt: %+v", built)
	}

	task, err := braintrust.NewEvaluator[string, string](client).Functions().Task(ctx, eval.FunctionOpts{Slug: "greet"})
	if err != nil {
		t.Fatal(err)
	}
	out, err := task(ctx, "world", &eval.TaskHooks{})
	if err != nil || out.Value != "echo: Say hello to world" {
		t.Fatalf("task output %+v, %v", out, err)
	}

	code, err := fns.Create(ctx, functions.CreateParams{
		ProjectID: pid, Name: "double", Slug: "double",
		FunctionData: map[string]any{"type": "code", "data": map[string]any{
			"type": "inline", "runtime_context": map[string]any{"runtime": "node", "version": "22"},
			"code": "function handler(x) { return x * 2; }",
		}},
	})
	if err != nil {
		t.Fatal(err)
	}
	doubled, err := fns.Invoke(ctx, code.ID, 21)
	if err != nil || doubled != float64(42) {
		t.Fatalf("invoke: %v, %v", doubled, err)
	}
}

func TestEvalWithARemoteLLMScorer(t *testing.T) {
	ctx := context.Background()
	client, tp, project := newClient(t)
	pid := rest(t, "POST", "/v1/project", map[string]any{"name": project})["id"].(string)
	if _, err := client.API().Functions().Create(ctx, functions.CreateParams{
		ProjectID: pid, Name: "judge", Slug: "judge", FunctionType: "scorer",
		FunctionData: map[string]any{"type": "prompt"},
		PromptData: map[string]any{
			"options": map[string]any{"model": "stub"},
			"parser":  map[string]any{"type": "llm_classifier", "use_cot": true, "choice_scores": map[string]any{"correct": 1, "wrong": 0}},
			// The stub model picks the choice its prompt names.
			"prompt": map[string]any{"type": "chat", "messages": []any{map[string]any{
				"role": "user", "content": "{{output}} is {{expected}}",
			}}},
		},
	}); err != nil {
		t.Fatal(err)
	}
	evaluator := braintrust.NewEvaluator[string, string](client)
	judge, err := evaluator.Functions().Scorer(ctx, eval.FunctionOpts{Slug: "judge"})
	if err != nil {
		t.Fatal(err)
	}
	result, err := evaluator.Run(ctx, eval.Opts[string, string]{
		Experiment: "remote-judge",
		Dataset: eval.NewDataset([]eval.Case[string, string]{
			{Input: "correct", Expected: "right"},
			{Input: "wrong", Expected: "right"},
		}),
		Task:    eval.T(func(ctx context.Context, in string) (string, error) { return in, nil }),
		Scorers: []eval.Scorer[string, string]{judge},
		Quiet:   true,
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := tp.ForceFlush(ctx); err != nil {
		t.Fatal(err)
	}
	var scores []float64
	for _, r := range events(t, "experiment", result.ID()) {
		if s, ok := score(r, "judge"); ok {
			scores = append(scores, s)
		}
	}
	sort.Float64s(scores)
	if len(scores) != 2 || scores[0] != 0 || scores[1] != 1 {
		t.Fatalf("scores: %v", scores)
	}
}

func score(row map[string]any, name string) (float64, bool) {
	scores, _ := row["scores"].(map[string]any)
	s, ok := scores[name].(float64)
	return s, ok
}
