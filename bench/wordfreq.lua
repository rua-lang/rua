local function text(reps)
  local words = {"the", "quick", "brown", "fox", "jumps", "over", "lazy", "dog",
                 "pack", "my", "box", "with", "five", "dozen", "liquor", "jugs"}
  local parts = {}
  for r = 0, reps - 1 do
    for i = 0, #words - 1 do parts[#parts + 1] = words[(i * 7 + r * 3) % #words + 1] end
  end
  return table.concat(parts, " ")
end

local reps = tonumber(arg and arg[1]) or 20000
local t = os.clock()
local body = text(reps)
local counts = {}
for w in string.gmatch(body, "[^ ]+") do
  counts[w] = (counts[w] or 0) + 1
end
local keys = {}
for k in pairs(counts) do keys[#keys + 1] = k end
table.sort(keys)
local total = 0
for _, k in ipairs(keys) do total = total + counts[k] end
print(string.format("distinct = %d  total = %d  first = %s:%d", #keys, total, keys[1], counts[keys[1]]))
print(string.format("# wordfreq reps=%d in %.3fs", reps, os.clock() - t))
