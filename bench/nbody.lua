local PI = 3.141592653589793
local SOLAR_MASS = 4.0 * PI * PI
local DAYS = 365.24

local function body(x, y, z, vx, vy, vz, m)
  return {x, y, z, vx * DAYS, vy * DAYS, vz * DAYS, m * SOLAR_MASS}
end

local function advance(bodies, dt)
  local n = #bodies
  for i = 1, n do
    local bi = bodies[i]
    for j = 1, n do
      if j > i then
        local bj = bodies[j]
        local dx, dy, dz = bi[1] - bj[1], bi[2] - bj[2], bi[3] - bj[3]
        local d2 = dx * dx + dy * dy + dz * dz
        local mag = dt / (d2 * math.sqrt(d2))
        local mi, mj = bi[7] * mag, bj[7] * mag
        bi[4] = bi[4] - dx * mj
        bi[5] = bi[5] - dy * mj
        bi[6] = bi[6] - dz * mj
        bj[4] = bj[4] + dx * mi
        bj[5] = bj[5] + dy * mi
        bj[6] = bj[6] + dz * mi
      end
    end
  end
  for i = 1, n do
    local b = bodies[i]
    b[1] = b[1] + dt * b[4]
    b[2] = b[2] + dt * b[5]
    b[3] = b[3] + dt * b[6]
  end
end

local function energy(bodies)
  local n = #bodies
  local e = 0.0
  for i = 1, n do
    local bi = bodies[i]
    e = e + 0.5 * bi[7] * (bi[4] * bi[4] + bi[5] * bi[5] + bi[6] * bi[6])
    for j = 1, n do
      if j > i then
        local bj = bodies[j]
        local dx, dy, dz = bi[1] - bj[1], bi[2] - bj[2], bi[3] - bj[3]
        e = e - bi[7] * bj[7] / math.sqrt(dx * dx + dy * dy + dz * dz)
      end
    end
  end
  return e
end

local bodies = {
  body(0, 0, 0, 0, 0, 0, 1),
  body(4.84143144246472090, -1.16032004402742839, -0.103622044471123109,
       0.00166007664274403694, 0.00769901118419740425, -0.0000690460016972063023, 0.000954791938424326609),
  body(8.34336671824457987, 4.12479856412430479, -0.403523417114321381,
       -0.00276742510726862411, 0.00499852801234917238, 0.0000230417297573763929, 0.000285885980666130812),
  body(12.8943695621391310, -15.1111514016986312, -0.223307578892655734,
       0.00296460137564761618, 0.00237847173959480950, -0.0000296589568540237556, 0.0000436624404335156298),
  body(15.3796971148509165, -25.9193146099879641, 0.179258772950371181,
       0.00268067772490389322, 0.00162824170038242295, -0.0000951592254519715870, 0.0000515138902046611451),
}

local px, py, pz = 0.0, 0.0, 0.0
for i = 1, #bodies do
  local b = bodies[i]
  px = px + b[4] * b[7]
  py = py + b[5] * b[7]
  pz = pz + b[6] * b[7]
end
local sun = bodies[1]
sun[4] = -px / SOLAR_MASS
sun[5] = -py / SOLAR_MASS
sun[6] = -pz / SOLAR_MASS

local steps = tonumber(arg and arg[1]) or 200000
local t = os.clock()
print(string.format("%.9f", energy(bodies)))
for i = 1, steps do advance(bodies, 0.01) end
print(string.format("%.9f", energy(bodies)))
print(string.format("# nbody %d steps in %.3fs", steps, os.clock() - t))
