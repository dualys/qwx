; Adapted from tree-sitter-d 0.8.2 queries/highlights.scm.
; Copyright 2024 Garrett D'Amore; used under the MIT License.
; Permission is hereby granted, free of charge, to any person obtaining a copy
; of this software and associated documentation files (the "Software"), to deal
; in the Software without restriction, including without limitation the rights
; to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
; copies of the Software, and to permit persons to whom the Software is
; furnished to do so, subject to the following conditions:
; The above copyright notice and this permission notice shall be included in
; all copies or substantial portions of the Software.
; THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
; IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
; FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
; AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
; LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
; OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
; SOFTWARE.

(string_literal) @string
(int_literal) @number
(float_literal) @number
(char_literal) @string
(identifier) @variable
(at_attribute) @attribute
(escape_sequence) @constant.character.escape

(function_declaration (identifier) @function)
(call_expression (identifier) @function)

[
  (lazy) (align) (extern) (static) (abstract) (final) (override)
  (synchronized) (auto) (scope) (gshared) (ref) (deprecated)
  (nothrow) (pure) (type_ctor)
] @keyword

[
  (alias) (asm) (assert) (cast) (class) (const) (debug) (delegate)
  (delete) (enum) (export) (function) (immutable) (import) (in)
  (inout) (interface) (invariant) (is) (mixin) (module) (new)
  (out) (package) (pragma) (private) (protected) (public) (shared)
  (struct) (super) (template) (this) (throw) (typeid) (typeof)
  (union) (unittest) (version) (with) (traits) (vector)
] @keyword

[
  (break) (case) (catch) (continue) (do) (default) (finally)
  (else) (for) (foreach) (foreach_reverse) (goto) (if) (switch)
  (try) (return) (while)
] @keyword.control

[
  (null) (true) (false) (special_keyword)
] @constant.builtin

[
  (void) (bool) (byte) (ubyte) (char) (short) (ushort) (wchar)
  (dchar) (int) (uint) (long) (ulong) (real) (double) (float)
  (size_t) (ptrdiff_t) (string) (cstring) (wstring) (noreturn)
] @type.builtin

(comment) @comment
(directive) @keyword.directive
(label (identifier) @label)
(type (identifier) @type)

[
  ";" "." ":" ","
] @punctuation.delimiter

[
  "(" ")" "[" "]" "{" "}"
] @punctuation.bracket
