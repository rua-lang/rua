local function fannkuch(n)
  local perm, perm1, count = {}, {}, {}
  for i = 0, n - 1 do perm[i] = 0 perm1[i] = i count[i] = 0 end
  local max_flips, checksum, r, permutations = 0, 0, n, 0
  while true do
    while r ~= 1 do count[r - 1] = r r = r - 1 end
    for i = 0, n - 1 do perm[i] = perm1[i] end
    local flips = 0
    local k = perm[0]
    while k ~= 0 do
      local i, j = 0, k
      while i < j do
        perm[i], perm[j] = perm[j], perm[i]
        i = i + 1
        j = j - 1
      end
      flips = flips + 1
      k = perm[0]
    end
    if flips > max_flips then max_flips = flips end
    if permutations % 2 == 0 then checksum = checksum + flips else checksum = checksum - flips end
    permutations = permutations + 1
    local done = false
    while true do
      if r == n then done = true break end
      local first = perm1[0]
      for i = 0, r - 1 do perm1[i] = perm1[i + 1] end
      perm1[r] = first
      count[r] = count[r] - 1
      if count[r] > 0 then break end
      r = r + 1
    end
    if done then break end
  end
  return checksum, max_flips
end

local n = tonumber(arg and arg[1]) or 9
local t = os.clock()
local checksum, flips = fannkuch(n)
print(checksum)
print(string.format("Pfannkuchen(%d) = %d", n, flips))
print(string.format("# fannkuch n=%d in %.3fs", n, os.clock() - t))
