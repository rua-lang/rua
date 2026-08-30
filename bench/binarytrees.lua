local function make(depth)
  if depth == 0 then return {l = nil, r = nil} end
  return {l = make(depth - 1), r = make(depth - 1)}
end

local function check(node)
  if node.l == nil then return 1 end
  return 1 + check(node.l) + check(node.r)
end

local max_depth = tonumber(arg and arg[1]) or 16
local min_depth = 4
local t = os.clock()

local stretch = max_depth + 1
print(string.format("stretch tree of depth %d\t check: %d", stretch, check(make(stretch))))

local long_lived = make(max_depth)
local d = min_depth
while d <= max_depth do
  local iterations = 1
  local e = max_depth - d + min_depth
  for i = 1, e do iterations = iterations * 2 end
  local sum = 0
  for i = 1, iterations do sum = sum + check(make(d)) end
  print(string.format("%d\t trees of depth %d\t check: %d", iterations, d, sum))
  d = d + 2
end
print(string.format("long lived tree of depth %d\t check: %d", max_depth, check(long_lived)))
print(string.format("# binarytrees depth %d in %.3fs", max_depth, os.clock() - t))
