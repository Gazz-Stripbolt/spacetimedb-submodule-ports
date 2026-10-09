// @spacetimedb/lobby port: the upstream matchmaking and room tests (scripts/test.ts and
// scripts/test-rooms.mjs at the pinned commit), ranked-selection / Elo / band vectors generated
// from the real TS code (tests/vectors/lobby.json), and the queue, room, rating, sweep and view
// behaviour end to end.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { call, callErr, procErr, sql, newIdentity, test, done, unwrap, sleep, some, none } from './lib.mjs';

const vectors = JSON.parse(readFileSync(new URL('./vectors/lobby.json', import.meta.url), 'utf8'));
const run = Date.now().toString(36);
const P = (name) => `${name}.${run}`; // pools unique per run
const opt = (v) => (v === undefined ? none : some(v));
const fromOpt = (o) => (o[0] === 0 ? o[1] : undefined);
// Options come back as [0, value] (some) or [1, []] (none), from procedures and SQL alike.
const SOME = (v) => [0, v];
const NONE = [1, []];
const hex = (who) => who.identity.toLowerCase().replace(/^0x/, '');

const join = async (pool, subject, size = 2, o = {}) => {
  const [ticketId, roomId] = unwrap(await call('demo_join_queue', [pool, subject, size, opt(o.attributes), opt(o.ttl)]));
  return { ticketId, roomId: fromOpt(roomId) };
};
const joinErr = (pool, subject, size = 2, o = {}) => procErr('demo_join_queue', [pool, subject, size, opt(o.attributes), opt(o.ttl)]);
const joinRanked = async (pool, subject, size = 2, o = {}) => {
  const [ticketId, roomId] = unwrap(await call('demo_join_ranked_queue', [pool, subject, size, none, opt(o.ttl), opt(o.ratingPool)]));
  return { ticketId, roomId: fromOpt(roomId) };
};
const ticket = async (id) => (await sql(`SELECT * FROM lobby_queue_ticket WHERE ticket_id = '${id}'`))[0];
const room = async (id) => (await sql(`SELECT * FROM lobby_room WHERE room_id = ${id}`))[0];
const seats = async (id) => sql(`SELECT * FROM lobby_room_seat WHERE room_id = ${id}`);
const rating = async (pool, subject) => (await sql(`SELECT * FROM lobby_subject_rating WHERE pool = '${pool}' AND subject = '${subject}'`))[0];
// Enum columns come back from SQL as { "<Variant>": [] } or [tag, []]; normalize to the name.
const tag = (v, names) => (Array.isArray(v) ? names[v[0]] : Object.keys(v)[0]);
const ROOM = ['Ready', 'Active', 'Closed', 'Abandoned'];
const TICKET = ['Queued', 'Matched', 'Cancelled', 'Expired'];
const SEAT = ['Reserved', 'Joined', 'Left'];
const roomStatus = async (id) => tag((await room(id)).status, ROOM);

/** Queue two subjects into a fresh pool and return the matched room id. */
async function matchedPair(pool, a = 'alice', b = 'bob') {
  await join(pool, a);
  const { roomId } = await join(pool, b);
  assert.ok(roomId, 'second ticket matches');
  return roomId;
}

// -- scripts/test.ts and generated vectors ------------------------------------------------------

await test('matchmaking: upstream cases (Elo, bands, ranked selection, composite keys)', async () => {
  const elo = async (r, o, s) => call('demo_elo', [r, o, s]);
  assert.equal((await elo(1000, 1000, 1))[0], 0.5);
  assert.ok((await elo(1200, 1000, 1))[0] > 0.75);
  assert.equal((await call('demo_elo', [1000, 1000, 1]))[1], 1016);
  assert.equal((await call('demo_elo', [1000, 1000, 0]))[1], 984);
  assert.equal((await call('demo_elo', [100, -10000, 0]))[1], 100, 'clamped to 100');
  assert.equal((await call('demo_elo', [5000, 10000, 1]))[1], 5000, 'clamped to 5000');
  assert.equal(await call('demo_band', [100_000_000, 100_000_000]), 100);
  assert.equal(await call('demo_band', [0, 20_000_000]), 200);
  assert.equal(await call('demo_band', [0, 1_000_000_000]), 800);
  const T = (id, rating, pool, at) => [id, some(rating), some(pool), at];
  const queue = [T('a', 1000, 'ranked', 0), T('b', 1080, 'ranked', 1), T('c', 1400, 'ranked', 2)];
  assert.deepEqual(await call('demo_ranked_selection', [queue, 2, 5_000_000]), SOME(['a', 'b']));
  assert.deepEqual(await call('demo_ranked_selection', [queue, 3, 5_000_000]), NONE);
  const mixed = [T('anchor', 1000, 'ranked', 0), T('far', 1090, 'ranked', 1), T('other', 1000, 'casual', 2), T('near', 960, 'ranked', 3)];
  assert.deepEqual(await call('demo_ranked_selection', [mixed, 2, 0]), SOME(['anchor', 'near']));
  assert.deepEqual(await call('demo_ranked_selection', [mixed, 3, 0]), SOME(['anchor', 'near', 'far']));
  const spread = Array.from({ length: 5000 }, (_, i) => T(`s${i}`, 100 + i * 1000, 'ranked', i));
  const started = Date.now();
  assert.deepEqual(await call('demo_ranked_selection', [spread, 2, 0]), NONE);
  assert.ok(Date.now() - started < 5000, 'far-apart ratings stay unmatched quickly at the scan cap');
  assert.notEqual(await call('demo_key', [['a:b', 'c']]), await call('demo_key', [['a', 'b:c']]));
  assert.equal(await call('demo_key', [['ranked', 'player-1']]), '6:ranked8:player-1');
});

await test(`vectors: ${vectors.selections.length} ranked selections, ${vectors.elo.length} Elo updates, bands and keys match TS`, async () => {
  for (const [i, v] of vectors.selections.entries()) {
    const tickets = v.tickets.map((t) => [t.ticketId, t.rating === null ? none : some(t.rating), t.ratingPool === null ? none : some(t.ratingPool), Number(t.createdAt)]);
    const got = await call('demo_ranked_selection', [tickets, v.matchSize, Number(v.now)]);
    assert.deepEqual(got, v.picked ? SOME(v.picked) : NONE, `selection ${i}`);
  }
  for (const v of vectors.elo) {
    const [expected, next] = await call('demo_elo', [v.rating, v.opponent, v.score]);
    assert.ok(Math.abs(expected - v.expected) < 1e-12, `expected ${v.rating} vs ${v.opponent}`);
    assert.equal(next, v.next, `next ${v.rating} vs ${v.opponent}, score ${v.score}`);
  }
  for (const v of vectors.bands) assert.equal(await call('demo_band', [Number(v.created), Number(v.now)]), v.band);
  for (const v of vectors.keys) assert.equal(await call('demo_key', [v.parts]), v.key);
});

// -- scripts/test-rooms.mjs ---------------------------------------------------------------------

await test('upstream rooms: join activates, a leaver cannot block the result, Elo updates, then closed', async () => {
  const pool = P('duel');
  const roomId = await matchedPair(pool);
  assert.equal(await roomStatus(roomId), 'Ready');
  await call('demo_join_room', [roomId, 'alice']);
  assert.equal(await roomStatus(roomId), 'Ready');
  await call('demo_join_room', [roomId, 'bob']);
  assert.equal(await roomStatus(roomId), 'Active');
  await call('demo_leave_room', [roomId, 'bob']);
  assert.equal(await roomStatus(roomId), 'Active', 'a leaver cannot stop the result');
  await call('demo_report_result', [roomId, some('alice')]);
  assert.equal(await roomStatus(roomId), 'Closed');
  const results = await sql(`SELECT * FROM lobby_match_result WHERE room_id = ${roomId}`);
  assert.equal(results.length, 1);
  assert.ok((await rating(pool, 'bob')).rating < 1000);
  assert.equal((await rating(pool, 'alice')).rating, 1016);
  assert.equal(await callErr('demo_report_result', [roomId, some('bob')]), 'lobby.room_not_active');
});

await test('upstream: expired tickets never match; expiry is the ticket timestamp plus TTL', async () => {
  const pool = P('expiry');
  const stale = await join(pool, 'carol', 2, { ttl: 1 });
  await sleep(1100);
  const dave = await join(pool, 'dave', 2, { ttl: 5 });
  assert.equal(dave.roomId, undefined, 'the stale ticket is not matched');
  const t = await ticket(dave.ticketId);
  assert.equal(BigInt(t.expires_at[0] ?? t.expires_at) - BigInt(t.created_at[0] ?? t.created_at), 5_000_000n);
  const erin = await join(pool, 'erin', 2);
  assert.ok(erin.roomId, 'live tickets still match');
  assert.equal(tag((await ticket(stale.ticketId)).status, TICKET), 'Queued', 'expired but not yet swept');
});

// -- the rest -----------------------------------------------------------------------------------

await test('queue: one ticket per subject, first-come matching of equal sizes, seats in order', async () => {
  const a = P('a');
  const b = P('b');
  const first = await join(a, 'frank');
  const second = await join(b, 'frank');
  assert.equal(tag((await ticket(first.ticketId)).status, TICKET), 'Cancelled', 'joining again cancels the queued ticket');
  assert.equal(tag((await ticket(second.ticketId)).status, TICKET), 'Queued');
  assert.match(second.ticketId, /^ticket:[0-9a-f-]{36}$/);
  const trio = P('trio');
  await join(trio, 'p1', 3);
  await join(trio, 'x1', 2);
  await join(trio, 'p2', 3);
  const { roomId } = await join(trio, 'p3', 3);
  assert.ok(roomId);
  const s = (await seats(roomId)).sort((x, y) => x.seat_index - y.seat_index);
  // Seats follow the index scan inside the matching transaction, which returns that transaction's
  // own insert (the newcomer) before committed rows. TS runs on the same engine, so it seats them
  // in this order too, not strictly oldest first. See README.
  assert.deepEqual(s.map((x) => x.subject), ['p3', 'p1', 'p2']);
  assert.ok(s.every((x) => tag(x.status, SEAT) === 'Reserved'));
  const r = await room(roomId);
  assert.equal(r.capacity, 3);
  assert.deepEqual(r.metadata_json, NONE, 'unranked rooms have no metadata');
});

await test('queue: validation', async () => {
  const pool = P('v');
  const cases = [
    [['  ', 'a'], 'lobby.invalid_pool'],
    [['p'.repeat(97), 'a'], 'lobby.invalid_pool'],
    [[pool, ''], 'lobby.invalid_subject'],
    [[pool, 's'.repeat(161)], 'lobby.invalid_subject'],
    [[pool, 'a', 0], 'lobby.invalid_match_size'],
    [[pool, 'a', 17], 'lobby.invalid_match_size'],
    [[pool, 'a', 2, { ttl: 0 }], 'lobby.invalid_ttl_seconds'],
    [[pool, 'a', 2, { ttl: 86_401 }], 'lobby.invalid_ttl_seconds'],
    [[pool, 'a', 2, { attributes: '[1]' }], 'lobby.invalid_attributes_json'],
    [[pool, 'a', 2, { attributes: 'null' }], 'lobby.invalid_attributes_json'],
    [[pool, 'a', 2, { attributes: '{bad' }], 'lobby.invalid_attributes_json'],
    [[pool, 'a', 2, { attributes: JSON.stringify({ x: 'y'.repeat(4100) }) }], 'lobby.invalid_attributes_json'],
  ];
  for (const [args, code] of cases) assert.equal(await joinErr(...args), code, JSON.stringify(args).slice(0, 60));
  const ok = await join(pool, '  gina ', 2, { attributes: '  { "region" : "eu" }  ' });
  const t = await ticket(ok.ticketId);
  assert.equal(t.subject, 'gina', 'trimmed');
  assert.deepEqual(t.attributes_json, SOME('{ "region" : "eu" }'), 'attributes kept as given (trimmed), not re-serialized');
});

await test('cancel: owner only, queued only', async () => {
  const pool = P('c');
  const { ticketId } = await join(pool, 'hank');
  assert.equal(await callErr('demo_cancel_ticket', [ticketId, 'ivy']), 'lobby.not_ticket_owner');
  assert.equal(await callErr('demo_cancel_ticket', ['ticket:missing', 'hank']), 'lobby.ticket_not_found');
  assert.equal(await callErr('demo_cancel_ticket', ['  ', 'hank']), 'lobby.invalid_ticket_id');
  await call('demo_cancel_ticket', [`  ${ticketId} `, 'hank']);
  assert.equal(await callErr('demo_cancel_ticket', [ticketId, 'hank']), 'lobby.ticket_not_queued');
});

await test('rooms: leaving a ready room abandons it; seat and room errors; close_room', async () => {
  const roomId = await matchedPair(P('r1'), 'jo', 'kim');
  assert.equal(await callErr('demo_join_room', [999_999, 'jo']), 'lobby.room_not_found');
  assert.equal(await callErr('demo_join_room', [roomId, 'nobody']), 'lobby.seat_not_found');
  await call('demo_leave_room', [roomId, 'jo']);
  assert.equal(await roomStatus(roomId), 'Abandoned');
  assert.equal(await callErr('demo_join_room', [roomId, 'kim']), 'lobby.room_closed');
  await call('demo_leave_room', [roomId, 'kim']); // terminal: no-op
  const r2 = await matchedPair(P('r2'), 'lee', 'max');
  await call('demo_join_room', [r2, 'lee']);
  await call('demo_join_room', [r2, 'lee']); // already joined: no-op
  await call('demo_join_room', [r2, 'max']);
  await call('demo_leave_room', [r2, 'max']);
  assert.equal(await callErr('demo_join_room', [r2, 'max']), 'lobby.seat_left');
  await call('demo_leave_room', [r2, 'lee']);
  assert.equal(await roomStatus(r2), 'Abandoned', 'an active room is abandoned once every seat has left');
  const r3 = await matchedPair(P('r3'), 'ned', 'oli');
  await call('demo_close_room', [r3]);
  assert.equal(await roomStatus(r3), 'Closed');
  assert.ok((await room(r3)).closed_at);
  await call('demo_close_room', [r3]);
  assert.equal(await callErr('demo_close_room', [999_999]), 'lobby.room_not_found');
});

await test('ranked: ratings snapshot into tickets, bands keep far ratings apart, results update the rating pool', async () => {
  const pool = P('ranked');
  const rp = P('season1');
  const stranger = await newIdentity();
  assert.equal(await callErr('lobby_set_rating', [rp, 'pat', 1200], stranger), 'lobby.not_authorized');
  assert.equal(await callErr('lobby_set_rating', [rp, 'pat', 99]), 'lobby.invalid_rating');
  assert.equal(await callErr('lobby_set_rating', [rp, 'pat', 5001]), 'lobby.invalid_rating');
  assert.equal(await callErr('lobby_set_rating', [' ', 'pat', 1200]), 'lobby.invalid_pool');
  await call('lobby_set_rating', [rp, 'pat', 1400]);
  await call('lobby_set_rating', [rp, 'quinn', 1000]);
  const p = await joinRanked(pool, 'pat', 2, { ratingPool: rp });
  const q = await joinRanked(pool, 'quinn', 2, { ratingPool: rp });
  assert.equal(q.roomId, undefined, '400 apart is outside the 100 band');
  assert.deepEqual((await ticket(p.ticketId)).rating, SOME(1400));
  const r = await joinRanked(pool, 'rae', 2, { ratingPool: rp });
  assert.ok(r.roomId, '1000 vs a new 1000 matches');
  const meta = (await room(r.roomId)).metadata_json;
  assert.deepEqual(meta, SOME(`{"ranked":true,"ratingPool":"${rp}"}`));
  const s = (await seats(r.roomId)).map((x) => x.subject).sort();
  assert.deepEqual(s, ['quinn', 'rae'], 'oldest anchor first, closest rating');
  await call('demo_join_room', [r.roomId, 'quinn']);
  await call('demo_join_room', [r.roomId, 'rae']);
  assert.equal(await callErr('demo_report_result', [r.roomId, some('pat')]), 'lobby.winner_not_in_room');
  assert.equal(await callErr('demo_report_result', [r.roomId, some('  ')]), 'lobby.invalid_winner_subject');
  await call('demo_report_result', [r.roomId, none]);
  const quinn = await rating(rp, 'quinn');
  const rae = await rating(rp, 'rae');
  assert.equal(quinn.draws, 1);
  assert.equal(rae.draws, 1);
  assert.equal(quinn.rating, 1000);
  const [result] = await sql(`SELECT * FROM lobby_match_result WHERE room_id = ${r.roomId}`);
  assert.equal(result.pool, rp);
  assert.deepEqual(result.winner_subject, NONE);
  // Defaults: the rating pool is the queue's pool.
  const d1 = await joinRanked(P('solo'), 'sam');
  const d2 = await joinRanked(P('solo'), 'tess');
  assert.deepEqual((await room(d2.roomId)).metadata_json, SOME(`{"ranked":true,"ratingPool":"${P('solo')}"}`));
  void d1;
});

await test('report: two seats required', async () => {
  const pool = P('three');
  await join(pool, 'u1', 3);
  await join(pool, 'u2', 3);
  const { roomId } = await join(pool, 'u3', 3);
  for (const s of ['u1', 'u2', 'u3']) await call('demo_join_room', [roomId, s]);
  assert.equal(await callErr('demo_report_result', [roomId, some('u1')]), 'lobby.result_requires_two_seats');
});

await test('client reducers and views: per caller, admin views for admins only', async () => {
  const pool = P('views');
  const alice = await newIdentity();
  const bob = await newIdentity();
  await call('lobby_join_queue', [pool, 2, none, none], alice);
  const mine = await sql('SELECT * FROM my_lobby_tickets', alice);
  assert.equal(mine.length, 1);
  assert.equal(mine[0].subject, hex(alice));
  assert.equal(await callErr('lobby_cancel_ticket', [mine[0].ticket_id], bob), 'lobby.not_ticket_owner');
  await call('lobby_join_queue', [pool, 2, none, none], bob);
  const [seat] = await sql('SELECT * FROM my_lobby_room_seats', alice);
  assert.ok(seat, 'matched seat visible to its subject');
  await call('lobby_join_room', [seat.room_id], alice);
  await call('lobby_join_room', [seat.room_id], bob);
  const rooms = await sql('SELECT * FROM my_lobby_rooms', bob);
  assert.equal(rooms.length, 1);
  assert.equal(tag(rooms[0].status, ROOM), 'Active');
  const summary = (await sql('SELECT * FROM lobby_queue_summary', bob)).find((r) => r.pool === pool);
  assert.deepEqual([summary.queued_tickets, summary.ready_rooms, summary.active_rooms], [0, 0, 1]);
  await call('lobby_leave_room', [seat.room_id], alice);
  await call('demo_report_result', [seat.room_id, some(hex(bob))]);
  const ratings = await sql('SELECT * FROM my_lobby_ratings', bob);
  assert.ok(ratings.some((r) => r.pool === pool && r.wins === 1));
  const board = await sql('SELECT * FROM lobby_ranked_leaderboard', bob);
  const inPool = board.filter((r) => r.pool === pool);
  assert.deepEqual(inPool.map((r) => r.subject), [hex(bob), hex(alice)], 'best first within a pool');
  for (const v of ['lobby_admin_tickets', 'lobby_admin_rooms', 'lobby_admin_room_seats', 'lobby_admin_match_results']) {
    assert.equal((await sql(`SELECT * FROM ${v}`, bob)).length, 0, `${v} hidden from non-admins`);
    assert.ok((await sql(`SELECT * FROM ${v}`)).length > 0, `${v} visible to admins`);
  }
  const status = unwrap(await call('get_lobby_status', []));
  assert.equal(status[0], 60);
  assert.equal(status[1], 16);
});

await test('config and sweep: expire, abandon stale ready rooms, delete past retention', async () => {
  const stranger = await newIdentity();
  assert.equal(await callErr('lobby_update_config', [60, 16, 120, 3600], stranger), 'lobby.not_authorized');
  for (const bad of [[0, 16, 120, 3600], [86_401, 16, 120, 3600], [60, 0, 120, 3600], [60, 129, 120, 3600], [60, 16, 0, 3600], [60, 16, 120, 0]]) {
    assert.equal(await callErr('lobby_update_config', bad), 'lobby.invalid_config', JSON.stringify(bad));
  }
  await call('lobby_update_config', [60, 16, 1, 2]);
  try {
    const pool = P('sweep');
    const lone = await join(pool, 'wren', 2, { ttl: 1 });
    const roomId = await matchedPair(P('sweep2'), 'xia', 'yan');
    const done_ = await matchedPair(P('sweep3'), 'zed', 'abe');
    await call('demo_close_room', [done_]);
    await sleep(1200);
    await call('demo_sweep', []);
    assert.equal(tag((await ticket(lone.ticketId)).status, TICKET), 'Expired');
    assert.equal(await roomStatus(roomId), 'Abandoned', 'ready past the timeout');
    await sleep(2200);
    await call('demo_sweep', []);
    assert.equal(await ticket(lone.ticketId), undefined, 'expired ticket deleted after retention');
    assert.equal(await room(done_), undefined, 'closed room deleted');
    assert.equal((await seats(done_)).length, 0, 'with its seats');
    assert.equal(await room(roomId), undefined);
  } finally {
    await call('lobby_update_config', [60, 16, 120, 3600]);
  }
});

await test('admins: add/remove, last admin protected; the scheduled sweep rejects clients', async () => {
  const stranger = await newIdentity();
  const sid = { __identity__: '0x' + stranger.identity };
  assert.equal(await callErr('add_lobby_admin', [sid], stranger), 'lobby.not_authorized');
  await call('add_lobby_admin', [sid]);
  await call('lobby_update_config', [60, 16, 120, 3600], stranger);
  await call('remove_lobby_admin', [sid]);
  assert.equal(await callErr('lobby_update_config', [60, 16, 120, 3600], stranger), 'lobby.not_authorized');
  const me = (await sql('SELECT * FROM lobby_admin_identity'))[0].identity;
  assert.equal(await callErr('remove_lobby_admin', [me]), 'lobby.cannot_remove_last_admin');
  assert.ok(await callErr('lobby_sweep', [[0, { Interval: [15_000_000] }]], stranger));
});

done();
