36	### Bundling tmux
37	
38	Single-binary deployment with embedded tmux: `.claude/docs/bundling-tmux.md`
39	
40	### Testing
41	
42	```bash
43	make build && make test     # Build (generates protos) then test
44	make quick-check            # Build + test + lint (fast validation)
45	make ci                     # Full CI pipeline (definitive pre-push check)
46	
47	go test ./server/services   # Specific packages (requires make build first)
48	go test ./ui -run TestFoo   # Specific test
49	make test-coverage
50	
51	# Frontend tests (not part of make ci)
52	cd web-app && npx jest --no-coverage
53	cd web-app && npx jest --testPathPatterns="<pattern>" --no-coverage
54	```
55	
56	Benchmark reference (all benchmarks MUST be run with `&`): `.claude/docs/benchmarks.md`
57	
58	### Code Quality
59	
60	```bash