-- A Scheme, written in Lua: reader, evaluator with proper tail calls, closures
-- and set!, and a workload written in the Scheme it implements. This is a
-- line-for-line translation of bench/lisp.rua, so both engines run the same
-- program and must print the same bytes.

-- ---------------------------------------------------------------- values
-- Tag 0 is a pair, 1 a closure, 2 a primitive, 3 the empty list. Symbols are
-- strings, numbers are numbers, booleans are booleans. Nothing else exists.
local NIL = { t = 3 }

local function cons(a, d) return { t = 0, a = a, d = d } end

local function is_pair(x) return type(x) == "table" and x.t == 0 end

-- An array of values becomes a list, right to left.
local function list_from(items, from)
    local out = NIL
    local k = #items
    while k >= from do
        out = cons(items[k], out)
        k = k - 1
    end
    return out
end

-- A list becomes an array, which is how closures keep their parameters and
-- bodies: the walk happens once, at closure creation, not at every call.
local function array_from(list)
    local out = {}
    local p = list
    while is_pair(p) do
        out[#out + 1] = p.a
        p = p.d
    end
    return out
end

local function list_len(list)
    local n = 0
    local p = list
    while is_pair(p) do
        n = n + 1
        p = p.d
    end
    return n
end

-- ----------------------------------------------------------- environments
-- A frame is a map from symbol to value plus a link to the enclosing frame.
-- Lookup walks the chain, which is where an interpreter of this shape spends
-- most of its time.
local function env_new(up) return { v = {}, up = up } end

local function env_get(env, name)
    local e = env
    while e ~= nil do
        local hit = e.v[name]
        if hit ~= nil then return hit end
        e = e.up
    end
    error("unbound symbol: " .. name)
end

local function env_set(env, name, val)
    local e = env
    while e ~= nil do
        if e.v[name] ~= nil then
            e.v[name] = val
            return name
        end
        e = e.up
    end
    error("set! on unbound symbol: " .. name)
end

-- ---------------------------------------------------------------- reader
local function is_space(c) return c == 32 or c == 9 or c == 10 or c == 13 end

local function tokenize(src)
    local toks = {}
    local i = 0
    local n = #src
    while i < n do
        local c = src:byte(i + 1)
        if c == 59 then                                -- ; comment to line end
            while i < n and src:byte(i + 1) ~= 10 do i = i + 1 end
        elseif is_space(c) then
            i = i + 1
        elseif c == 40 or c == 41 or c == 39 then      -- ( ) '
            toks[#toks + 1] = src:sub(i + 1, i + 1)
            i = i + 1
        else
            local start = i
            while i < n do
                local d = src:byte(i + 1)
                if is_space(d) or d == 40 or d == 41 or d == 59 then break end
                i = i + 1
            end
            toks[#toks + 1] = src:sub(start + 1, i)
        end
    end
    return toks
end

local function atom(tok)
    if tok == "#t" then return true end
    if tok == "#f" then return false end
    local c = tok:byte(1)
    if (c >= 48 and c <= 57) or (c == 45 and #tok > 1) then
        local v = tonumber(tok)
        if v ~= nil then return v end
    end
    return tok
end

-- Returns the datum and the index just past it.
local parse_at
parse_at = function(toks, i)
    if i > #toks then error("unexpected end of input") end
    local tok = toks[i]
    if tok == "(" then
        i = i + 1
        local items = {}
        while true do
            if i > #toks then error("missing )") end
            if toks[i] == ")" then break end
            local v, j = parse_at(toks, i)
            items[#items + 1] = v
            i = j
        end
        return list_from(items, 1), i + 1
    end
    if tok == "'" then
        local v, j = parse_at(toks, i + 1)
        return cons("quote", cons(v, NIL)), j
    end
    if tok == ")" then error("unexpected )") end
    return atom(tok), i + 1
end

local function parse_all(src)
    local toks = tokenize(src)
    local forms = {}
    local i = 1
    while i <= #toks do
        local v, j = parse_at(toks, i)
        forms[#forms + 1] = v
        i = j
    end
    return forms
end

-- -------------------------------------------------------------- printing
-- rua prints an integral double as an integer; match that, in a way that
-- works on both 5.4 and LuaJIT.
local function numstr(x)
    if x == math.floor(x) and x > -1e15 and x < 1e15 then
        return string.format("%d", x)
    end
    return tostring(x)
end

local write_str
write_str = function(x)
    local ty = type(x)
    if ty == "number" then return numstr(x) end
    if ty == "string" then return x end
    if ty == "boolean" then if x then return "#t" else return "#f" end end
    if x.t == 3 then return "()" end
    if x.t == 1 then return "#<procedure " .. x.n .. ">" end
    if x.t == 2 then return "#<primitive " .. x.n .. ">" end
    local out = "("
    local p = x
    local first = true
    while is_pair(p) do
        if not first then out = out .. " " end
        out = out .. write_str(p.a)
        first = false
        p = p.d
    end
    if p ~= NIL then out = out .. " . " .. write_str(p) end
    return out .. ")"
end

-- ------------------------------------------------------------ evaluation
local function closure(params, body, env, name)
    return { t = 1, p = array_from(params), b = array_from(body), e = env, n = name }
end

-- Scheme truthiness: #f alone is false, and zero is not.
local function truthy(v) return v ~= false end

local eval
eval = function(x, env)
    while true do
        do
            local tx = type(x)
            if tx == "string" then return env_get(env, x) end
            if tx ~= "table" then return x end          -- numbers, booleans
            if x.t ~= 0 then return x end               -- (), closures, prims
            local op = x.a

            if type(op) == "string" then
                if op == "quote" then return x.d.a end

                if op == "if" then
                    if truthy(eval(x.d.a, env)) then
                        x = x.d.d.a
                    else
                        local alt = x.d.d.d
                        if alt == NIL then return NIL end
                        x = alt.a
                    end
                    goto continue
                end

                if op == "define" then
                    local target = x.d.a
                    if is_pair(target) then             -- (define (f a) body...)
                        local name = target.a
                        env.v[name] = closure(target.d, x.d.d, env, name)
                        return name
                    end
                    env.v[target] = eval(x.d.d.a, env)
                    return target
                end

                if op == "set!" then return env_set(env, x.d.a, eval(x.d.d.a, env)) end

                if op == "lambda" then return closure(x.d.a, x.d.d, env, "lambda") end

                if op == "begin" then
                    local body = x.d
                    if body == NIL then return NIL end
                    while body.d ~= NIL do
                        eval(body.a, env)
                        body = body.d
                    end
                    x = body.a
                    goto continue
                end

                if op == "let" then                     -- (let ((a 1)) body...)
                    local inner = env_new(env)
                    local b = x.d.a
                    while is_pair(b) do
                        inner.v[b.a.a] = eval(b.a.d.a, env)
                        b = b.d
                    end
                    env = inner
                    x = cons("begin", x.d.d)
                    goto continue
                end

                if op == "cond" then
                    local clause = x.d
                    local taken = nil
                    while is_pair(clause) do
                        local test = clause.a.a
                        if test == "else" or truthy(eval(test, env)) then
                            taken = clause.a.d
                            break
                        end
                        clause = clause.d
                    end
                    if taken == nil then return NIL end
                    x = cons("begin", taken)
                    goto continue
                end

                if op == "and" then
                    local p = x.d
                    if p == NIL then return true end
                    while p.d ~= NIL do
                        if not truthy(eval(p.a, env)) then return false end
                        p = p.d
                    end
                    x = p.a
                    goto continue
                end

                if op == "or" then
                    local p = x.d
                    if p == NIL then return false end
                    while p.d ~= NIL do
                        local v = eval(p.a, env)
                        if truthy(v) then return v end
                        p = p.d
                    end
                    x = p.a
                    goto continue
                end
            end

            -- application
            local f = eval(op, env)
            local args = {}
            local p = x.d
            while p ~= NIL do
                args[#args + 1] = eval(p.a, env)
                p = p.d
            end

            if type(f) ~= "table" then error("not a procedure: " .. write_str(f)) end
            if f.t == 2 then
                local prim = f.f
                return prim(args)
            end
            if f.t ~= 1 then error("not a procedure: " .. write_str(f)) end

            -- a call in tail position reuses this loop rather than the host stack
            local inner = env_new(f.e)
            local names = f.p
            local k = 1
            while k <= #names do
                inner.v[names[k]] = args[k]
                k = k + 1
            end
            local body = f.b
            local last = #body
            local j = 1
            while j < last do
                eval(body[j], inner)
                j = j + 1
            end
            env = inner
            x = body[last]
        end
        ::continue::
    end
end

-- ------------------------------------------------------------ primitives
local function prim(env, name, f) env.v[name] = { t = 2, n = name, f = f } end

local function install(env)
    prim(env, "+", function(a)
        local s = 0
        for k = 1, #a do s = s + a[k] end
        return s
    end)
    prim(env, "-", function(a)
        if #a == 1 then return 0 - a[1] end
        local s = a[1]
        for k = 2, #a do s = s - a[k] end
        return s
    end)
    prim(env, "*", function(a)
        local s = 1
        for k = 1, #a do s = s * a[k] end
        return s
    end)
    prim(env, "quotient", function(a) local q = a[1] / a[2] if q < 0 then return 0 - math.floor(0 - q) else return math.floor(q) end end)
    prim(env, "remainder", function(a) local q = a[1] / a[2] if q < 0 then q = 0 - math.floor(0 - q) else q = math.floor(q) end return a[1] - a[2] * q end)
    prim(env, "modulo", function(a) return a[1] % a[2] end)
    prim(env, "=", function(a) return a[1] == a[2] end)
    prim(env, "<", function(a) return a[1] < a[2] end)
    prim(env, ">", function(a) return a[1] > a[2] end)
    prim(env, "<=", function(a) return a[1] <= a[2] end)
    prim(env, ">=", function(a) return a[1] >= a[2] end)
    prim(env, "abs", function(a) if a[1] < 0 then return 0 - a[1] else return a[1] end end)
    prim(env, "min", function(a) if a[1] < a[2] then return a[1] else return a[2] end end)
    prim(env, "max", function(a) if a[1] > a[2] then return a[1] else return a[2] end end)
    prim(env, "cons", function(a) return cons(a[1], a[2]) end)
    prim(env, "car", function(a) return a[1].a end)
    prim(env, "cdr", function(a) return a[1].d end)
    prim(env, "list", function(a) return list_from(a, 1) end)
    prim(env, "length", function(a) return list_len(a[1]) end)
    prim(env, "null?", function(a) return a[1] == NIL end)
    prim(env, "pair?", function(a) return is_pair(a[1]) end)
    prim(env, "number?", function(a) return type(a[1]) == "number" end)
    prim(env, "symbol?", function(a) return type(a[1]) == "string" end)
    prim(env, "procedure?", function(a) return type(a[1]) == "table" and (a[1].t == 1 or a[1].t == 2) end)
    prim(env, "not", function(a) return a[1] == false end)
    prim(env, "eq?", function(a) return a[1] == a[2] end)
    prim(env, "reverse", function(a)
        local out = NIL
        local p = a[1]
        while is_pair(p) do
            out = cons(p.a, out)
            p = p.d
        end
        return out
    end)
    prim(env, "append", function(a)
        local items = {}
        local p = a[1]
        while is_pair(p) do
            items[#items + 1] = p.a
            p = p.d
        end
        local out = a[2]
        local k = #items
        while k >= 1 do
            out = cons(items[k], out)
            k = k - 1
        end
        return out
    end)
    prim(env, "list-ref", function(a)
        local p = a[1]
        local k = a[2]
        while k > 0 do
            p = p.d
            k = k - 1
        end
        return p.a
    end)
    prim(env, "write", function(a) print(write_str(a[1])) return a[1] end)
end

-- ---------------------------------------------------------- the workload
-- Scheme source, held as lines so that both files can carry it verbatim.
local function program()
    local lines = {
        "(define (tak x y z)",
        "  (if (not (< y x)) z",
        "      (tak (tak (- x 1) y z) (tak (- y 1) z x) (tak (- z 1) x y))))",
        "",
        "(define (fib n) (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2)))))",
        "",
        "; tail recursive, so the evaluator must not grow the host stack",
        "(define (sum-to i n acc) (if (> i n) acc (sum-to (+ i 1) n (+ acc i))))",
        "",
        "(define (iota-loop i n acc) (if (= i n) (reverse acc) (iota-loop (+ i 1) n (cons i acc))))",
        "(define (iota n) (iota-loop 0 n '()))",
        "",
        "(define (map1 f xs) (if (null? xs) '() (cons (f (car xs)) (map1 f (cdr xs)))))",
        "(define (filter1 p xs)",
        "  (cond ((null? xs) '())",
        "        ((p (car xs)) (cons (car xs) (filter1 p (cdr xs))))",
        "        (else (filter1 p (cdr xs)))))",
        "(define (fold f init xs) (if (null? xs) init (fold f (f init (car xs)) (cdr xs))))",
        "",
        "; a small congruential generator: every product stays exact in a double",
        "(define (next-rand s) (remainder (+ (* s 75) 74) 65537))",
        "(define (rand-loop s n acc)",
        "  (if (= n 0) acc (rand-loop (next-rand s) (- n 1) (cons (remainder s 1000) acc))))",
        "(define (rand-list n seed) (rand-loop seed n '()))",
        "",
        "; merge sort, both halves built with an accumulator so recursion stays shallow",
        "(define (split-loop xs n left)",
        "  (if (= n 0) (cons (reverse left) xs)",
        "      (split-loop (cdr xs) (- n 1) (cons (car xs) left))))",
        "(define (merge-loop a b acc)",
        "  (cond ((null? a) (append (reverse acc) b))",
        "        ((null? b) (append (reverse acc) a))",
        "        ((< (car a) (car b)) (merge-loop (cdr a) b (cons (car a) acc)))",
        "        (else (merge-loop a (cdr b) (cons (car b) acc)))))",
        "(define (msort xs)",
        "  (let ((n (length xs)))",
        "    (if (< n 2) xs",
        "        (let ((halves (split-loop xs (quotient n 2) '())))",
        "          (merge-loop (msort (car halves)) (msort (cdr halves)) '())))))",
        "",
        "(define (sorted? xs)",
        "  (cond ((null? xs) #t)",
        "        ((null? (cdr xs)) #t)",
        "        ((> (car xs) (car (cdr xs))) #f)",
        "        (else (sorted? (cdr xs)))))",
        "",
        "; closures over a mutable cell",
        "(define (make-counter start)",
        "  (lambda (by) (set! start (+ start by)) start))",
        "(define (drive-counter c n acc)",
        "  (if (= n 0) acc (drive-counter c (- n 1) (c n))))",
        "",
        "; association lists, keyed by symbol",
        "(define (make-alist n acc)",
        "  (if (= n 0) acc (make-alist (- n 1) (cons (cons (list n) n) acc))))",
        "(define (assoc-num k al)",
        "  (cond ((null? al) -1)",
        "        ((= (car (car (car al))) k) (cdr (car al)))",
        "        (else (assoc-num k (cdr al)))))",
        "(define (probe-loop al i n acc)",
        "  (if (> i n) acc (probe-loop al (+ i 1) n (+ acc (assoc-num (remainder i 40) al)))))",
        "",
        "; eight queens, over lists",
        "(define (safe? col dist placed)",
        "  (cond ((null? placed) #t)",
        "        ((= (car placed) col) #f)",
        "        ((= (abs (- (car placed) col)) dist) #f)",
        "        (else (safe? col (+ dist 1) (cdr placed)))))",
        "(define (try-col col n placed count)",
        "  (if (> col n) count",
        "      (try-col (+ col 1) n placed",
        "               (if (safe? col 1 placed)",
        "                   (+ count (queens-from (cons col placed) n))",
        "                   count))))",
        "(define (queens-from placed n)",
        "  (if (= (length placed) n) 1 (try-col 1 n placed 0)))",
        "(define (queens n) (queens-from '() n))",
        "",
        "; the Y combinator, because a closure test should include the hard one",
        "(define Y",
        "  (lambda (f)",
        "    ((lambda (x) (f (lambda (v) ((x x) v))))",
        "     (lambda (x) (f (lambda (v) ((x x) v)))))))",
        "(define fact",
        "  (Y (lambda (self) (lambda (n) (if (= n 0) 1 (* n (self (- n 1))))))))",
        "",
        "(define (main)",
        "  (let ((nums (rand-list 240 7)))",
        "    (let ((sorted (msort nums)))",
        "      (list",
        "        (tak 18 12 6)",
        "        (fib 19)",
        "        (sum-to 1 20000 0)",
        "        (fold (lambda (a b) (+ a b)) 0 (map1 (lambda (x) (* x x)) (iota 120)))",
        "        (length (filter1 (lambda (x) (= (remainder x 3) 0)) (iota 120)))",
        "        (sorted? sorted)",
        "        (list-ref sorted 0)",
        "        (list-ref sorted 239)",
        "        (fold (lambda (a b) (+ a b)) 0 sorted)",
        "        (drive-counter (make-counter 0) 400 0)",
        "        (probe-loop (make-alist 40 '()) 1 300 0)",
        "        (queens 6)",
        "        (fact 12)",
        "        (length (append (iota 100) (reverse (iota 100))))))))",
        "",
    }
    return table.concat(lines, "\n")
end

-- ------------------------------------------------------------------ main
local reps = (arg and arg[1]) and tonumber(arg[1]) or 8
local t = os.clock()

local genv = env_new(nil)
install(genv)
local forms = parse_all(program())
for k = 1, #forms do eval(forms[k], genv) end

local call = parse_at(tokenize("(main)"), 1)
local result = NIL
for r = 1, reps do result = eval(call, genv) end

local p = result
local i = 0
while is_pair(p) do
    print(string.format("%d: %s", i, write_str(p.a)))
    p = p.d
    i = i + 1
end
print(string.format("# lisp reps=%d in %.3fs", reps, os.clock() - t))
