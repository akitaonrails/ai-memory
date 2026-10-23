#!/bin/sh
# POSIX sh + awk port of score_proposal.py for ai-memory auto-improve eval gates.
# It needs no Python, so it also runs inside the official Docker image, which
# ships only a POSIX shell and awk. The checks never match section names or
# wording, so a wiki written in any language scores the same way.

LC_ALL=C exec awk '
function fail(reason) {
	printf "{\"passed\":false,\"reason\":\"%s\"}\n", reason
	exit 0
}

function skip_space() {
	while (pos <= len && index(" \t\r\n", substr(doc, pos, 1)) > 0) pos++
}

# Decodes the JSON string starting at doc[pos] (the opening quote) and leaves
# pos after the closing quote. It finds the closing quote first and decodes the
# whole run with gsub: appending one character at a time copies the string on
# every step, which took 22 s on a 1 MB page. \uXXXX becomes "?": the checks
# only look at ASCII markup, and the server writes non-ASCII text as raw UTF-8.
function read_string(    start, c, raw) {
	start = ++pos
	while (pos <= len) {
		c = substr(doc, pos, 1)
		if (c == "\"") break
		pos += c == "\\" ? 2 : 1
	}
	if (pos > len) fail("invalid input JSON: unterminated string")
	raw = substr(doc, start, pos - start)
	pos++
	gsub(/\\\\/, "\001", raw)
	gsub(/\\n/, "\n", raw)
	gsub(/\\t/, "\t", raw)
	gsub(/\\r/, "\r", raw)
	gsub(/\\[bf]/, " ", raw)
	gsub(/\\u[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]/, "?", raw)
	gsub(/\\"/, "\"", raw)
	gsub(/\\\//, "/", raw)
	gsub(/\001/, "\\", raw)
	return raw
}

# Skips a non-string value (number, literal, nested object or array).
function skip_value(    depth, c) {
	depth = 0
	while (pos <= len) {
		c = substr(doc, pos, 1)
		if (c == "\"") { read_string(); continue }
		if (c == "{" || c == "[") depth++
		else if (c == "}" || c == "]") { if (depth == 0) return; depth-- }
		else if (c == "," && depth == 0) return
		pos++
	}
}

function read_top_level_fields(    key, c) {
	pos = 1
	skip_space()
	if (substr(doc, pos, 1) != "{") fail("invalid input JSON: expected an object")
	pos++
	while (pos <= len) {
		skip_space()
		c = substr(doc, pos, 1)
		if (c == "}") return
		if (c == ",") { pos++; continue }
		if (c != "\"") fail("invalid input JSON: expected a key")
		key = read_string()
		skip_space()
		if (substr(doc, pos, 1) != ":") fail("invalid input JSON: expected a colon")
		pos++
		skip_space()
		if (substr(doc, pos, 1) == "\"") field[key] = read_string()
		else skip_value()
	}
	fail("invalid input JSON: unterminated object")
}

function utf8_length(text,    copy, continuation_bytes) {
	copy = text
	continuation_bytes = gsub(/[\200-\277]/, "", copy)
	return length(text) - continuation_bytes
}

function trimmed(text) {
	sub(/^[ \t\r\n]+/, "", text)
	sub(/[ \t\r\n]+$/, "", text)
	return text
}

# Same checks and weights as score_proposal.py. Sets reasons as a side effect.
function score_body(path, body,    lines, n, i, line, h1, h2, item, prose, placeholder, score) {
	n = split(body, lines, "\n")
	for (i = 1; i <= n; i++) {
		line = lines[i]
		if (line ~ /^#[ \t]+[^ \t]/) h1 = 1
		if (line ~ /^##[ \t]+[^ \t]/) h2 = 1
		if (line ~ /^[ \t]*([-*+]|[0-9]+[.)])[ \t]+[^ \t]/) item = 1
		if (line ~ /[^ \t]/ && line !~ /^[ \t]*#/) prose = 1
		if (line ~ /(^|[^A-Za-z0-9_])(TODO|TBD)([^A-Za-z0-9_]|$)/) placeholder = 1
	}
	score = 0
	reasons = ""
	if (h1) score += 0.25; else add_reason("missing H1 heading")
	if (!placeholder) score += 0.20; else add_reason("contains TODO/TBD placeholder")
	if (path ~ /^procedures\//) {
		if (h2) score += 0.20; else add_reason("procedure has no ## section")
		if (item) score += 0.25; else add_reason("procedure has no list of steps")
	} else if (path ~ /^_rules\//) {
		if (prose) score += 0.35; else add_reason("rule has no text beyond its heading")
		if (utf8_length(trimmed(body)) <= 2400) score += 0.10
		else add_reason("rule is too long for an agent instruction")
	} else {
		score += 0.45
	}
	return score > 1 ? 1 : score
}

function add_reason(reason) {
	reasons = reasons == "" ? reason : reasons "; " reason
}

{ doc = doc $0 "\n" }

END {
	len = length(doc)
	read_top_level_fields()
	score_before = score_body(field["path"], field["before_body"])
	score_after = score_body(field["path"], field["after_body"])
	passed = score_after >= 0.70 && score_after >= score_before
	printf "{\"passed\":%s,\"score_before\":%.4f,\"score_after\":%.4f", \
		passed ? "true" : "false", score_before, score_after
	if (!passed) printf ",\"reason\":\"%s\"", reasons == "" ? "score did not improve" : reasons
	printf "}\n"
}
'
