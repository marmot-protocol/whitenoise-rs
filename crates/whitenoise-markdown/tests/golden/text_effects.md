# Text effects

Plain {big}large text{/big} and {small}tiny text{/small} inline.

A {shake}shaky **bold** and *italic*{/shake} run mixes markdown.

Arbitrary names work: {WHATEVER}anything goes{/WHATEVER}.

Nesting different effects: {big}outer {shake}inner{/shake} tail{/big}.

Same-name nesting: {big}a {big}b{/big} c{/big}.

Effects do not fire inside code: `{big}literal{/big}` stays a code span,
and {big}an effect wrapping a `{/big}` code span{/big} closes correctly.

```
{big}fenced code is untouched{/big}
```

Non-effects stay literal: {1, 2, 3}, {not closed, and \{big\}escaped\{/big\}.
