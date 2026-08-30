local function matrix(n, seed)
  local m = {}
  local s = seed
  for i = 0, n - 1 do
    local row = {}
    for j = 0, n - 1 do
      s = (s * 16807) % 2147483647
      row[j] = s % 100
    end
    m[i] = row
  end
  return m
end

local function multiply(a, b, n)
  local out = {}
  for i = 0, n - 1 do
    local row = {}
    local ai = a[i]
    for j = 0, n - 1 do
      local sum = 0
      for k = 0, n - 1 do sum = sum + ai[k] * b[k][j] end
      row[j] = sum
    end
    out[i] = row
  end
  return out
end

local n = tonumber(arg and arg[1]) or 200
local t = os.clock()
local a = matrix(n, 1)
local b = matrix(n, 7)
local c = multiply(a, b, n)
local trace = 0
for i = 0, n - 1 do trace = trace + c[i][i] end
print(string.format("trace = %d", trace))
print(string.format("# matmul n=%d in %.3fs", n, os.clock() - t))
