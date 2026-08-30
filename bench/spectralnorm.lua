local function a(i, j) return 1.0 / ((i + j) * (i + j + 1) / 2 + i + 1) end

local function av(x, y, n)
  for i = 0, n - 1 do
    local s = 0.0
    for j = 0, n - 1 do s = s + a(i, j) * x[j] end
    y[i] = s
  end
end

local function atv(x, y, n)
  for i = 0, n - 1 do
    local s = 0.0
    for j = 0, n - 1 do s = s + a(j, i) * x[j] end
    y[i] = s
  end
end

local function at_av(x, y, t, n) av(x, t, n) atv(t, y, n) end

local n = tonumber(arg and arg[1]) or 500
local t0 = os.clock()
local u, v, tmp = {}, {}, {}
for i = 0, n - 1 do u[i] = 1.0 v[i] = 0.0 tmp[i] = 0.0 end
for i = 1, 10 do at_av(u, v, tmp, n) at_av(v, u, tmp, n) end
local vbv, vv = 0.0, 0.0
for i = 0, n - 1 do vbv = vbv + u[i] * v[i] vv = vv + v[i] * v[i] end
print(string.format("%.9f", math.sqrt(vbv / vv)))
print(string.format("# spectralnorm n=%d in %.3fs", n, os.clock() - t0))
