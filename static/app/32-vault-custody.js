/* ── Credential vault (credential custody v1) ──
   The user's provider credentials, end-to-end encrypted client-side and
   stored as an opaque blob on the daemon used by this trusted dashboard.
   Connect retains an account-vault storage API for compatibility, but the
   default hosted directory serves no vault client and has no bridge to a
   daemon. A random
   256-bit master key encrypts the vault body; that key is stored only
   wrapped, one envelope per enrolled unlocker: each account passkey (via
   the vault's OWN WebAuthn PRF salt 'intendant-vault-v1', evaluated as
   the second salt of the login gesture — a PRF domain separate from
   fleet-sync, so the two features never share key material; markerless
   legacy envelopes derived from the fleet secret migrate on unlock) and
   a mandatory BIP39 recovery phrase. Losing a passkey loses one envelope, not the vault. Every blob
   carries an HMAC under a key derived from the master key (see "Vault
   blob authentication" below), so the store cannot tamper, splice, or
   relabel. What remains to the store: withholding, or serving a stale
   revision — detectable via the local high-water mark. Unsealing happens
   only client-side, in memory, behind a passkey gesture or the phrase —
   and the key-touching crypto itself runs inside the pinned crypto
   kernel (static/vault-kernel.js, a hash-verified dedicated Worker; see
   "Vault crypto: the pinned kernel" below), so the master key never
   exists in this page's memory. */

const VAULT_LOCAL_KEY = 'intendant_vault_local_v1';
const VAULT_HKDF_SALT = 'intendant-vault-v1';
/* Dedicated vault PRF secret (salt 'intendant-vault-v1', evaluated as the
   `second` PRF salt in the same passkey gesture as fleet-sync's `first`).
   Envelopes wrapped under it carry `prf: 'vault-v1'`; markerless prf
   envelopes are legacy (KEK derived from the FLEET secret) and are
   re-wrapped onto the dedicated domain at first unlock. */
const VAULT_PRF_SESSION_KEY = 'intendant_vault_prf_v1';
const VAULT_PRF_ENVELOPE_MARK = 'vault-v1';
/* The standard BIP39 English wordlist (2048 words). The phrase is
   generated client-side — the server never sees it. */
const VAULT_BIP39_WORDLIST = 'abandon ability able about above absent absorb abstract absurd abuse access accident account accuse achieve acid acoustic acquire across act action actor actress actual adapt add addict address adjust admit adult advance advice aerobic affair afford afraid again age agent agree ahead aim air airport aisle alarm album alcohol alert alien all alley allow almost alone alpha already also alter always amateur amazing among amount amused analyst anchor ancient anger angle angry animal ankle announce annual another answer antenna antique anxiety any apart apology appear apple approve april arch arctic area arena argue arm armed armor army around arrange arrest arrive arrow art artefact artist artwork ask aspect assault asset assist assume asthma athlete atom attack attend attitude attract auction audit august aunt author auto autumn average avocado avoid awake aware away awesome awful awkward axis baby bachelor bacon badge bag balance balcony ball bamboo banana banner bar barely bargain barrel base basic basket battle beach bean beauty because become beef before begin behave behind believe below belt bench benefit best betray better between beyond bicycle bid bike bind biology bird birth bitter black blade blame blanket blast bleak bless blind blood blossom blouse blue blur blush board boat body boil bomb bone bonus book boost border boring borrow boss bottom bounce box boy bracket brain brand brass brave bread breeze brick bridge brief bright bring brisk broccoli broken bronze broom brother brown brush bubble buddy budget buffalo build bulb bulk bullet bundle bunker burden burger burst bus business busy butter buyer buzz cabbage cabin cable cactus cage cake call calm camera camp can canal cancel candy cannon canoe canvas canyon capable capital captain car carbon card cargo carpet carry cart case cash casino castle casual cat catalog catch category cattle caught cause caution cave ceiling celery cement census century cereal certain chair chalk champion change chaos chapter charge chase chat cheap check cheese chef cherry chest chicken chief child chimney choice choose chronic chuckle chunk churn cigar cinnamon circle citizen city civil claim clap clarify claw clay clean clerk clever click client cliff climb clinic clip clock clog close cloth cloud clown club clump cluster clutch coach coast coconut code coffee coil coin collect color column combine come comfort comic common company concert conduct confirm congress connect consider control convince cook cool copper copy coral core corn correct cost cotton couch country couple course cousin cover coyote crack cradle craft cram crane crash crater crawl crazy cream credit creek crew cricket crime crisp critic crop cross crouch crowd crucial cruel cruise crumble crunch crush cry crystal cube culture cup cupboard curious current curtain curve cushion custom cute cycle dad damage damp dance danger daring dash daughter dawn day deal debate debris decade december decide decline decorate decrease deer defense define defy degree delay deliver demand demise denial dentist deny depart depend deposit depth deputy derive describe desert design desk despair destroy detail detect develop device devote diagram dial diamond diary dice diesel diet differ digital dignity dilemma dinner dinosaur direct dirt disagree discover disease dish dismiss disorder display distance divert divide divorce dizzy doctor document dog doll dolphin domain donate donkey donor door dose double dove draft dragon drama drastic draw dream dress drift drill drink drip drive drop drum dry duck dumb dune during dust dutch duty dwarf dynamic eager eagle early earn earth easily east easy echo ecology economy edge edit educate effort egg eight either elbow elder electric elegant element elephant elevator elite else embark embody embrace emerge emotion employ empower empty enable enact end endless endorse enemy energy enforce engage engine enhance enjoy enlist enough enrich enroll ensure enter entire entry envelope episode equal equip era erase erode erosion error erupt escape essay essence estate eternal ethics evidence evil evoke evolve exact example excess exchange excite exclude excuse execute exercise exhaust exhibit exile exist exit exotic expand expect expire explain expose express extend extra eye eyebrow fabric face faculty fade faint faith fall false fame family famous fan fancy fantasy farm fashion fat fatal father fatigue fault favorite feature february federal fee feed feel female fence festival fetch fever few fiber fiction field figure file film filter final find fine finger finish fire firm first fiscal fish fit fitness fix flag flame flash flat flavor flee flight flip float flock floor flower fluid flush fly foam focus fog foil fold follow food foot force forest forget fork fortune forum forward fossil foster found fox fragile frame frequent fresh friend fringe frog front frost frown frozen fruit fuel fun funny furnace fury future gadget gain galaxy gallery game gap garage garbage garden garlic garment gas gasp gate gather gauge gaze general genius genre gentle genuine gesture ghost giant gift giggle ginger giraffe girl give glad glance glare glass glide glimpse globe gloom glory glove glow glue goat goddess gold good goose gorilla gospel gossip govern gown grab grace grain grant grape grass gravity great green grid grief grit grocery group grow grunt guard guess guide guilt guitar gun gym habit hair half hammer hamster hand happy harbor hard harsh harvest hat have hawk hazard head health heart heavy hedgehog height hello helmet help hen hero hidden high hill hint hip hire history hobby hockey hold hole holiday hollow home honey hood hope horn horror horse hospital host hotel hour hover hub huge human humble humor hundred hungry hunt hurdle hurry hurt husband hybrid ice icon idea identify idle ignore ill illegal illness image imitate immense immune impact impose improve impulse inch include income increase index indicate indoor industry infant inflict inform inhale inherit initial inject injury inmate inner innocent input inquiry insane insect inside inspire install intact interest into invest invite involve iron island isolate issue item ivory jacket jaguar jar jazz jealous jeans jelly jewel job join joke journey joy judge juice jump jungle junior junk just kangaroo keen keep ketchup key kick kid kidney kind kingdom kiss kit kitchen kite kitten kiwi knee knife knock know lab label labor ladder lady lake lamp language laptop large later latin laugh laundry lava law lawn lawsuit layer lazy leader leaf learn leave lecture left leg legal legend leisure lemon lend length lens leopard lesson letter level liar liberty library license life lift light like limb limit link lion liquid list little live lizard load loan lobster local lock logic lonely long loop lottery loud lounge love loyal lucky luggage lumber lunar lunch luxury lyrics machine mad magic magnet maid mail main major make mammal man manage mandate mango mansion manual maple marble march margin marine market marriage mask mass master match material math matrix matter maximum maze meadow mean measure meat mechanic medal media melody melt member memory mention menu mercy merge merit merry mesh message metal method middle midnight milk million mimic mind minimum minor minute miracle mirror misery miss mistake mix mixed mixture mobile model modify mom moment monitor monkey monster month moon moral more morning mosquito mother motion motor mountain mouse move movie much muffin mule multiply muscle museum mushroom music must mutual myself mystery myth naive name napkin narrow nasty nation nature near neck need negative neglect neither nephew nerve nest net network neutral never news next nice night noble noise nominee noodle normal north nose notable note nothing notice novel now nuclear number nurse nut oak obey object oblige obscure observe obtain obvious occur ocean october odor off offer office often oil okay old olive olympic omit once one onion online only open opera opinion oppose option orange orbit orchard order ordinary organ orient original orphan ostrich other outdoor outer output outside oval oven over own owner oxygen oyster ozone pact paddle page pair palace palm panda panel panic panther paper parade parent park parrot party pass patch path patient patrol pattern pause pave payment peace peanut pear peasant pelican pen penalty pencil people pepper perfect permit person pet phone photo phrase physical piano picnic picture piece pig pigeon pill pilot pink pioneer pipe pistol pitch pizza place planet plastic plate play please pledge pluck plug plunge poem poet point polar pole police pond pony pool popular portion position possible post potato pottery poverty powder power practice praise predict prefer prepare present pretty prevent price pride primary print priority prison private prize problem process produce profit program project promote proof property prosper protect proud provide public pudding pull pulp pulse pumpkin punch pupil puppy purchase purity purpose purse push put puzzle pyramid quality quantum quarter question quick quit quiz quote rabbit raccoon race rack radar radio rail rain raise rally ramp ranch random range rapid rare rate rather raven raw razor ready real reason rebel rebuild recall receive recipe record recycle reduce reflect reform refuse region regret regular reject relax release relief rely remain remember remind remove render renew rent reopen repair repeat replace report require rescue resemble resist resource response result retire retreat return reunion reveal review reward rhythm rib ribbon rice rich ride ridge rifle right rigid ring riot ripple risk ritual rival river road roast robot robust rocket romance roof rookie room rose rotate rough round route royal rubber rude rug rule run runway rural sad saddle sadness safe sail salad salmon salon salt salute same sample sand satisfy satoshi sauce sausage save say scale scan scare scatter scene scheme school science scissors scorpion scout scrap screen script scrub sea search season seat second secret section security seed seek segment select sell seminar senior sense sentence series service session settle setup seven shadow shaft shallow share shed shell sheriff shield shift shine ship shiver shock shoe shoot shop short shoulder shove shrimp shrug shuffle shy sibling sick side siege sight sign silent silk silly silver similar simple since sing siren sister situate six size skate sketch ski skill skin skirt skull slab slam sleep slender slice slide slight slim slogan slot slow slush small smart smile smoke smooth snack snake snap sniff snow soap soccer social sock soda soft solar soldier solid solution solve someone song soon sorry sort soul sound soup source south space spare spatial spawn speak special speed spell spend sphere spice spider spike spin spirit split spoil sponsor spoon sport spot spray spread spring spy square squeeze squirrel stable stadium staff stage stairs stamp stand start state stay steak steel stem step stereo stick still sting stock stomach stone stool story stove strategy street strike strong struggle student stuff stumble style subject submit subway success such sudden suffer sugar suggest suit summer sun sunny sunset super supply supreme sure surface surge surprise surround survey suspect sustain swallow swamp swap swarm swear sweet swift swim swing switch sword symbol symptom syrup system table tackle tag tail talent talk tank tape target task taste tattoo taxi teach team tell ten tenant tennis tent term test text thank that theme then theory there they thing this thought three thrive throw thumb thunder ticket tide tiger tilt timber time tiny tip tired tissue title toast tobacco today toddler toe together toilet token tomato tomorrow tone tongue tonight tool tooth top topic topple torch tornado tortoise toss total tourist toward tower town toy track trade traffic tragic train transfer trap trash travel tray treat tree trend trial tribe trick trigger trim trip trophy trouble truck true truly trumpet trust truth try tube tuition tumble tuna tunnel turkey turn turtle twelve twenty twice twin twist two type typical ugly umbrella unable unaware uncle uncover under undo unfair unfold unhappy uniform unique unit universe unknown unlock until unusual unveil update upgrade uphold upon upper upset urban urge usage use used useful useless usual utility vacant vacuum vague valid valley valve van vanish vapor various vast vault vehicle velvet vendor venture venue verb verify version very vessel veteran viable vibrant vicious victory video view village vintage violin virtual virus visa visit visual vital vivid vocal voice void volcano volume vote voyage wage wagon wait walk wall walnut want warfare warm warrior wash wasp waste water wave way wealth weapon wear weasel weather web wedding weekend weird welcome west wet whale what wheat wheel when where whip whisper wide width wife wild will win window wine wing wink winner winter wire wisdom wise wish witness wolf woman wonder wood wool word work world worry worth wrap wreck wrestle wrist write wrong yard year yellow you young youth zebra zero zone zoo';
let vaultBip39Words = null;

function vaultWordlist() {
  if (!vaultBip39Words) vaultBip39Words = VAULT_BIP39_WORDLIST.split(' ');
  return vaultBip39Words;
}

const vaultState = {
  status: 'unknown',   // unknown | unavailable | signed-out | none | locked | unlocked
  revision: 0,         // revision of the blob this device considers current
  highWater: 0,        // highest revision this device has ever seen (rollback detection)
  blob: null,          // the full encrypted vault blob
  entries: [],         // decrypted entries while unlocked
  settings: {},        // decrypted vault-wide settings while unlocked
  // The master key itself lives in the crypto kernel worker while
  // unlocked; the page holds only vaultKernelToken (below).
  matchedEnvelopeId: null, // which prf envelope this session's passkey opened
  macSeen: false,      // downgrade ratchet: an authenticated blob has been seen
  rollbackWarning: '',
  lastError: '',
  migratedVoiceKeys: false,
};
let vaultCeremony = null;          // { phrase } while the create ceremony is on screen
let vaultPublishChain = Promise.resolve(true);
const vaultRevealedEntries = new Set();

/* ── Vault storage backends ──
   The shipped dashboard uses the daemon store (~/.intendant/vault-blob.json
   via api_daemon_vault_fetch/publish), with no Connect service in the loop.
   The dormant 'account' branch is wire-compatibility scaffolding for a
   future, separately trusted client; Connect does not serve this bundle.
   The stores are independent (each keeps its own revision ratchet), and a
   future copy between them must remain an explicit user action. */
function vaultDaemonStoreReady() {
  if (DASHBOARD_CONNECT_MODE) return false; // retired hosted-dashboard mode
  // daemonApi availability (transport F6): the tunnel status boolean when
  // it has landed (false = this session's role is refused the custody
  // gate), the hello_ack features list before that (absent = the daemon
  // predates local vault storage), optimistic only while the handshake is
  // still in flight — the pre-facade "let the RPCs answer" semantics.
  return daemonApi.availability('api_daemon_vault_fetch').ok;
}

/* Why the daemon store is out of reach, for the 'unavailable' status line
   — the honest split the conflated pre-facade copy could not make. */
function vaultDaemonStoreUnavailableText() {
  const reason = daemonApi.availability('api_daemon_vault_fetch').reason;
  if (reason === 'denied') {
    return "This session's role lacks credentials.manage, so this daemon's local vault store is out of reach. Reopen it through an authorized loopback/direct-mTLS session or grant that permission from a trusted root surface.";
  }
  if (reason === 'unsupported') {
    return 'This daemon predates local vault storage — upgrade it to keep a sealed vault here. Hosted Connect is discovery-only and cannot supply a vault client.';
  }
  // 'transport-down': the store rides the secure control channel (a
  // tunnel-only method). Whether that channel is even coming decides the
  // honest copy — "retries automatically" was a lie on dashboards that
  // never open one.
  if (!dashboardControlTransportEnabled()) {
    return 'The local vault store needs the secure dashboard control channel, and this dashboard has not enabled one. Enable it under Access → Diagnostics or use a trusted loopback/direct-mTLS dashboard.';
  }
  const status = dashboardTransport?.status
    ? dashboardTransport.status()
    : { enabled: true, connected: false };
  if (dashboardTransportStatusSummary(status).kind === 'err') {
    return 'The control channel to this daemon failed, so its local vault store is out of reach — see Access → Diagnostics. Hosted Connect cannot bridge an account vault to this daemon.';
  }
  return 'No vault store reachable yet: the trusted control channel is still connecting (retries automatically).';
}

function vaultBackendKind() {
  if (DASHBOARD_CONNECT_MODE) return 'account';
  return vaultDaemonStoreReady() ? 'daemon' : null;
}

function vaultAvailable() {
  return Boolean(crypto?.subtle) && vaultBackendKind() !== null;
}

/* ── Vault crypto: the pinned kernel ──
   All key-touching crypto lives in static/vault-kernel.js — a small,
   separately served, dedicated Worker that holds the master key, every
   KEK, and the MAC key for its lifetime (the kernel's header carries the
   full design and its honest limits). The page refuses to run vault
   crypto through unverified code: it fetches /vault-kernel.js, hashes
   the bytes, and compares against VAULT_KERNEL_SHA256 — the sha256 the
   app.html assembler computed from the kernel file at build time. On a
   mismatch the vault fails closed with a loud error; there is
   deliberately no inline-crypto fallback. What remains in this fragment
   is policy and state: which envelope to try, when to trust a MAC, what
   to render, storage and sync. */

/* Substituted by crates/app-html-assembler at assembly time (the
   fragment source carries the placeholder; the assembled app.html
   carries the real lowercase-hex sha256 of static/vault-kernel.js). */
const VAULT_KERNEL_SHA256 = '__VAULT_KERNEL_SHA256__';

let vaultKernel = null;        // { worker, pending, seq } once verified + running
let vaultKernelPromise = null; // in-flight spawn: concurrent callers share one fetch
let vaultKernelToken = '';     // the kernel's unlock token (page-held session nonce)

async function vaultKernelSpawn() {
  const resp = await fetch('/vault-kernel.js', { cache: 'no-cache' });
  if (!resp.ok) throw new Error(`vault kernel fetch failed: HTTP ${resp.status}`);
  const bytes = await resp.arrayBuffer();
  const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', bytes));
  const hex = Array.from(digest, b => b.toString(16).padStart(2, '0')).join('');
  if (hex !== VAULT_KERNEL_SHA256) {
    // Fail closed and loudly: a mismatch means the served kernel is not
    // the one this bundle was built against — tampering, or a skewed
    // deploy (e.g. an edited kernel without `cargo run -p
    // app-html-assembler`).
    const err = new Error(
      `VAULT KERNEL INTEGRITY FAILURE: /vault-kernel.js hashes to ${hex} but this dashboard pins ${VAULT_KERNEL_SHA256}. Refusing to unseal the vault through unverified code.`
    );
    err.vaultKernelIntegrity = true;
    throw err;
  }
  // Instantiate from the VERIFIED bytes (a blob: URL), never from the
  // network path — re-fetching could race a swap after verification.
  const url = URL.createObjectURL(new Blob([bytes], { type: 'text/javascript' }));
  let worker;
  try {
    worker = new Worker(url);
  } finally {
    URL.revokeObjectURL(url);
  }
  const kernel = { worker, pending: new Map(), seq: 0 };
  const fail = message => {
    for (const request of kernel.pending.values()) request.reject(new Error(message));
    kernel.pending.clear();
    if (vaultKernel === kernel) {
      vaultKernel = null;
      vaultKernelPromise = null;
    }
    try { worker.terminate(); } catch (_) {}
  };
  worker.onmessage = event => {
    const msg = event.data || {};
    const request = kernel.pending.get(msg.id);
    if (!request) return;
    kernel.pending.delete(msg.id);
    if (msg.ok) request.resolve(msg.result || {});
    else request.reject(new Error(msg.error || 'vault kernel error'));
  };
  worker.onerror = event => fail(`vault kernel crashed: ${event?.message || 'worker error'}`);
  worker.onmessageerror = () => fail('vault kernel message deserialization failed');
  return kernel;
}

function vaultKernelEnsure() {
  if (vaultKernel) return Promise.resolve(vaultKernel);
  if (!vaultKernelPromise) {
    vaultKernelPromise = vaultKernelSpawn().then(
      kernel => {
        vaultKernel = kernel;
        return kernel;
      },
      err => {
        vaultKernelPromise = null;
        if (err?.vaultKernelIntegrity) {
          // Tamper evidence must surface even on silent auto-unlock paths.
          console.error('[vault]', err.message);
          vaultState.lastError = err.message;
          renderAccessVaultSection();
        }
        throw err;
      }
    );
  }
  return vaultKernelPromise;
}

/* One kernel RPC. `transfer` optionally moves ArrayBuffers into the
   worker, detaching the page-side copy. */
async function vaultKernelCall(op, params = {}, transfer = []) {
  const kernel = await vaultKernelEnsure();
  return new Promise((resolve, reject) => {
    const id = ++kernel.seq;
    kernel.pending.set(id, { resolve, reject });
    try {
      kernel.worker.postMessage({ id, op, params }, transfer);
    } catch (err) {
      kernel.pending.delete(id);
      reject(err);
    }
  });
}

/* Drop the kernel's master key; best-effort and non-blocking so lock
   paths stay synchronous. Clears the page-held token either way, and
   never spawns a kernel just to lock it. */
function vaultKernelLock() {
  if (vaultKernel) vaultKernelCall('lock').catch(() => {});
  vaultKernelToken = '';
}

/* The dedicated vault PRF secret for this session, as bytes (null when
   the login gesture did not evaluate the second salt). sessionStorage
   keeps the base64url copy so a reload can re-unlock without a fresh
   gesture — a pre-kernel design the kernel does not change; what the
   kernel removes from the page is every key DERIVED from it. */
function vaultPrfSecretDedicated() {
  const prfB64u = sessionStorage.getItem(VAULT_PRF_SESSION_KEY) || '';
  if (!prfB64u) return null;
  try {
    return dashboardBase64UrlToBytes(prfB64u);
  } catch {
    return null;
  }
}

/* The fleet PRF secret — legacy: pre-two-salt envelopes were wrapped
   under a KEK derived from it. Kept for unlocking them (and as the wrap
   fallback for authenticators that only evaluate one PRF salt). */
function vaultPrfSecretLegacy() {
  const prfB64u = sessionStorage.getItem(FLEET_PRF_SESSION_KEY) || '';
  if (!prfB64u) return null;
  try {
    return dashboardBase64UrlToBytes(prfB64u);
  } catch {
    return null;
  }
}

/* The PRF secret + envelope marker for wrapping a NEW envelope:
   dedicated domain when the authenticator evaluated both salts, legacy
   (markerless) otherwise. */
function vaultPrfWrapSource() {
  const dedicated = vaultPrfSecretDedicated();
  if (dedicated) return [dedicated, VAULT_PRF_ENVELOPE_MARK];
  return [vaultPrfSecretLegacy(), null];
}

async function vaultGeneratePhrase() {
  const words = vaultWordlist();
  const entropy = crypto.getRandomValues(new Uint8Array(16));
  const hash = new Uint8Array(await crypto.subtle.digest('SHA-256', entropy));
  let bits = '';
  for (const b of entropy) bits += b.toString(2).padStart(8, '0');
  bits += hash[0].toString(2).padStart(8, '0').slice(0, 4);
  const picked = [];
  for (let i = 0; i < 12; i++) picked.push(words[parseInt(bits.slice(i * 11, (i + 1) * 11), 2)]);
  return picked.join(' ');
}

/* Normalize + checksum-verify a typed phrase; null when it is not a
   valid 12-word BIP39 mnemonic (catches typos before a doomed unwrap). */
async function vaultNormalizePhrase(input) {
  const words = String(input || '').toLowerCase().trim().split(/[\s,]+/).filter(Boolean);
  if (words.length !== 12) return null;
  const list = vaultWordlist();
  let bits = '';
  for (const word of words) {
    const idx = list.indexOf(word);
    if (idx < 0) return null;
    bits += idx.toString(2).padStart(11, '0');
  }
  const entropy = new Uint8Array(16);
  for (let i = 0; i < 16; i++) entropy[i] = parseInt(bits.slice(i * 8, (i + 1) * 8), 2);
  const hash = new Uint8Array(await crypto.subtle.digest('SHA-256', entropy));
  if (bits.slice(128) !== hash[0].toString(2).padStart(8, '0').slice(0, 4)) return null;
  return words.join(' ');
}

function vaultRandomId(prefix) {
  return `${prefix}_${dashboardBytesToBase64Url(crypto.getRandomValues(new Uint8Array(9)))}`;
}

/* ── Vault blob authentication ──
   The body AAD binds the revision, but nothing bound the ENVELOPE SET to
   the blob — a malicious store could splice a stale envelope set (say,
   one still containing a revoked passkey) onto the newest body. The MAC
   closes that: HMAC-SHA-256 under a key derived from the vault master
   key over the whole blob. The store never holds the master key, so it
   can neither mint nor relabel a MAC'd blob; every unlocker can verify.
   The kernel computes and verifies MACs (compute-mac / verify-mac); this
   page decides the POLICY — when a missing MAC is legacy-acceptable and
   when it is a downgrade attack (the macSeen ratchet below). */

/* Once this device has seen an authenticated blob, an unauthenticated one
   is a downgrade attack, not a legacy vault. Ratchet state rides in
   vaultState and persists through vaultWriteLocal. */
function vaultMarkMacSeen() {
  if (!vaultState.macSeen) {
    vaultState.macSeen = true;
    vaultWriteLocal();
  }
}

/* ── Local cache + backing-store sync ── */

function vaultReadLocal() {
  try {
    const parsed = JSON.parse(localStorage.getItem(VAULT_LOCAL_KEY) || 'null');
    return parsed && typeof parsed === 'object' ? parsed : null;
  } catch {
    return null;
  }
}

function vaultWriteLocal() {
  try {
    localStorage.setItem(VAULT_LOCAL_KEY, JSON.stringify({
      revision: vaultState.revision,
      high_water: vaultState.highWater,
      mac_seen: Boolean(vaultState.macSeen),
      vault: vaultState.blob,
      updated_unix_ms: Date.now(),
    }));
  } catch (err) {
    console.warn('[vault] local cache write failed:', err?.message || err);
  }
}

async function vaultServerFetch() {
  if (vaultBackendKind() === 'daemon') {
    const result = await vaultLeaseRpc('api_daemon_vault_fetch');
    return {
      authenticated: true,
      revision: Number(result?.revision) || 0,
      vault: result?.vault || null,
    };
  }
  // The account store lives on the Connect service, not on any daemon —
  // deliberately raw fetch, outside the daemonApi facade (the facade
  // speaks to daemons only; rendezvous calls stay on their own lane).
  const resp = await fetch(accessFleetHostedUrl('/api/vault'));
  if (resp.status === 401) return { authenticated: false };
  const body = await resp.json().catch(() => ({}));
  if (!resp.ok || body.ok === false) throw new Error(body.error || `HTTP ${resp.status}`);
  return { authenticated: true, revision: Number(body.revision) || 0, vault: body.vault || null };
}

async function vaultServerPublish(blob) {
  if (vaultBackendKind() === 'daemon') {
    try {
      return await vaultLeaseRpc('api_daemon_vault_publish', {
        revision: blob.revision,
        vault: blob,
      });
    } catch (err) {
      // The daemon store's conflict travels as an error string; give it
      // the same shape the hosted store's HTTP 409 gets.
      if (/revision conflict|stale vault/i.test(String(err?.message || ''))) {
        err.vaultConflict = true;
      }
      throw err;
    }
  }
  // Rides the shared hosted-POST helper (42-usage-terminal.js): one
  // CSRF-expiry retry, so a rotated session token cannot strand publishes.
  const resp = await accessFleetHostedPost('/api/vault',
    JSON.stringify({ revision: blob.revision, vault: blob }));
  if (!resp) throw new Error('sign in to the hosted account first');
  const body = await resp.json().catch(() => ({}));
  if (resp.status === 409) {
    const err = new Error(body.error || 'vault revision conflict');
    err.vaultConflict = true;
    throw err;
  }
  if (!resp.ok || body.ok === false) throw new Error(body.error || `HTTP ${resp.status}`);
  return body;
}

/* Take a fetched blob as current if it is newer than what we hold; flag
   rollback when the store serves older than this device has seen. */
async function vaultAdoptBlob(blob, { fromServer = false } = {}) {
  const revision = Number(blob?.revision) || 0;
  if (!blob || !revision) return;
  if (fromServer) {
    vaultState.rollbackWarning = vaultState.highWater > revision
      ? `The backing store returned vault revision ${revision}, but this device has already seen revision ${vaultState.highWater}. The store cannot read or forge the vault, but it can withhold updates — treat its copy as stale.`
      : '';
  }
  if (revision < vaultState.revision) return;
  // Authenticate before adopting. Unlocked: verify the MAC outright.
  // Locked with a MAC: adopt provisionally — unlock verifies before any
  // use. Either way the downgrade ratchet refuses an unauthenticated
  // blob once this device has seen an authenticated one.
  if (blob.mac && vaultKernelToken) {
    const { valid } = await vaultKernelCall('verify-mac', { token: vaultKernelToken, blob });
    if (!valid) {
      vaultState.lastError = 'The store returned a vault blob that failed its integrity check — ignoring it.';
      renderAccessVaultSection();
      return;
    }
    vaultMarkMacSeen();
  } else if (!blob.mac && vaultState.macSeen) {
    vaultState.lastError = 'The store served an unauthenticated vault although this device has seen an authenticated one — refusing the downgrade.';
    renderAccessVaultSection();
    return;
  }
  vaultState.blob = blob;
  vaultState.revision = revision;
  vaultState.highWater = Math.max(vaultState.highWater, revision);
  if (vaultKernelToken) {
    const { body } = await vaultKernelCall('decrypt-body', { token: vaultKernelToken, blob });
    if (body) {
      vaultState.entries = Array.isArray(body.entries) ? body.entries : [];
      vaultState.settings = body.settings && typeof body.settings === 'object' ? body.settings : {};
      vaultSyncVoiceMirror();
    } else {
      vaultLock('The vault was re-keyed on another device — unlock it again.');
    }
  } else if (vaultState.status !== 'unlocked') {
    vaultState.status = 'locked';
  }
  vaultWriteLocal();
}

let vaultInitPromise = null;
function vaultInit() {
  if (!vaultInitPromise) {
    vaultInitPromise = vaultInitInner().catch(err => {
      console.warn('[vault] init failed:', err?.message || err);
    });
  }
  return vaultInitPromise;
}

async function vaultInitInner() {
  if (!vaultAvailable()) {
    vaultState.status = 'unavailable';
    renderAccessVaultSection();
    return;
  }
  const local = vaultReadLocal();
  vaultState.macSeen = Boolean(local?.mac_seen);
  if (local?.vault) {
    vaultState.highWater = Math.max(Number(local.high_water) || 0, Number(local.vault.revision) || 0);
    await vaultAdoptBlob(local.vault);
  }
  vaultState.status = vaultState.blob ? 'locked' : 'none';
  try {
    const server = await vaultServerFetch();
    if (server.authenticated === false) {
      if (!vaultState.blob) vaultState.status = 'signed-out';
    } else if (server.vault) {
      await vaultAdoptBlob(server.vault, { fromServer: true });
    }
  } catch (err) {
    console.warn('[vault] hosted fetch failed (working from the local cache):', err?.message || err);
  }
  if (vaultState.blob && vaultState.status !== 'unlocked') {
    await vaultTryPrfUnlock({ silent: true });
  }
  renderAccessVaultSection();
}

/* ── Lock / unlock / create / enroll ── */

function vaultLock(notice) {
  vaultKernelLock();
  vaultState.entries = [];
  vaultState.settings = {};
  vaultState.matchedEnvelopeId = null;
  vaultRevealedEntries.clear();
  // A locked vault cannot serve relays: withdraw this tab's egress
  // registrations so the daemon fails over to a clear error.
  if (vaultEgressState.registered.size && vaultLeaseTransportReady()) {
    vaultLeaseRpc('api_credential_egress_unregister', {}).catch(() => {});
  }
  vaultEgressState.registered.clear();
  if (vaultState.blob) vaultState.status = 'locked';
  vaultState.lastError = notice || '';
  vaultSyncVoiceMirror();
  renderAccessVaultSection();
}

async function vaultFinishUnlock(token, envelopeId) {
  // Every unlock funnels through here: the kernel already holds the
  // master key behind `token`; authenticate the whole blob before
  // trusting any of it (and re-lock the kernel on every refusal so the
  // worker never outlives the page's decision). A blob without a MAC is
  // legacy — allowed only until this device has seen an authenticated
  // one, and upgraded in place right after unlock.
  vaultKernelToken = token;
  let body;
  try {
    if (vaultState.blob?.mac) {
      const { valid } = await vaultKernelCall('verify-mac', { token, blob: vaultState.blob });
      if (!valid) {
        vaultKernelLock();
        vaultState.lastError = 'Vault integrity check failed — the stored blob was tampered with or spliced. Refusing to unlock it.';
        renderAccessVaultSection();
        return false;
      }
      vaultMarkMacSeen();
    } else if (vaultState.macSeen) {
      vaultKernelLock();
      vaultState.lastError = 'The store served an unauthenticated vault although this device has seen an authenticated one — refusing the downgrade.';
      renderAccessVaultSection();
      return false;
    }
    body = (await vaultKernelCall('decrypt-body', { token, blob: vaultState.blob })).body;
  } catch (err) {
    // Kernel died mid-unlock: keep the historical never-throws contract.
    vaultKernelLock();
    vaultState.lastError = `Vault unlock failed: ${err?.message || err}`;
    renderAccessVaultSection();
    return false;
  }
  if (!body) {
    vaultKernelLock();
    vaultState.lastError = 'The key envelope opened but the vault body did not decrypt — the blob may be corrupted.';
    return false;
  }
  vaultState.entries = Array.isArray(body.entries) ? body.entries : [];
  vaultState.settings = body.settings && typeof body.settings === 'object' ? body.settings : {};
  vaultState.matchedEnvelopeId = envelopeId || null;
  vaultState.status = 'unlocked';
  vaultState.lastError = '';
  vaultSyncVoiceMirror();
  vaultMigrateVoiceKeys().catch(err => console.warn('[vault] voice key migration failed:', err?.message || err));
  if (!vaultState.blob?.mac) {
    // Legacy pre-MAC blob: upgrade it in place now that we hold the
    // master key. Ride the publish chain (serialized with user edits) but
    // fail quietly — the next save signs anyway.
    vaultPublishChain = vaultPublishChain
      .then(() => vaultPersist())
      .then(() => {
        console.info('[vault] legacy blob upgraded with an integrity MAC');
        return true;
      })
      .catch(err => {
        console.warn('[vault] MAC upgrade deferred (will retry on next save):', err?.message || err);
        return false;
      });
  }
  renderAccessVaultSection();
  vaultDepositLaneSync().catch(() => {});
  return true;
}

async function vaultTryPrfUnlock({ silent = false } = {}) {
  if (!vaultState.blob) return false;
  // Each envelope names its PRF domain: marked = dedicated vault salt,
  // markerless = legacy fleet-secret derivation. The kernel picks the
  // matching secret per envelope; this page only decides which secrets
  // exist this session. No secret at all skips the kernel entirely.
  const secretDedicated = vaultPrfSecretDedicated();
  const secretLegacy = vaultPrfSecretLegacy();
  if (!secretDedicated && !secretLegacy) {
    if (!silent) {
      vaultState.lastError = 'No passkey secret in this session — sign in with the account passkey, or use the recovery phrase.';
    }
    return false;
  }
  let result;
  try {
    // Transfer the secret buffers into the worker (detaches the page-side
    // copies; the sessionStorage strings remain for reload-unlock).
    const transfer = [secretDedicated?.buffer, secretLegacy?.buffer].filter(Boolean);
    result = await vaultKernelCall('unlock-prf', {
      envelopes: vaultState.blob.envelopes || [],
      secret_dedicated: secretDedicated,
      secret_legacy: secretLegacy,
    }, transfer);
  } catch (err) {
    console.warn('[vault] kernel unlock-prf failed:', err?.message || err);
    if (!silent && !err?.vaultKernelIntegrity) {
      vaultState.lastError = `Passkey unlock failed: ${err?.message || err}`;
    }
    return false;
  }
  if (result.unlocked) {
    const unlocked = await vaultFinishUnlock(result.token, result.envelope_id);
    if (unlocked && result.envelope_prf !== VAULT_PRF_ENVELOPE_MARK) {
      vaultMigratePrfEnvelope(result.envelope_id);
    }
    return unlocked;
  }
  if (!silent) {
    vaultState.lastError = result.saw_kek
      ? 'This passkey is not enrolled in the vault yet — unlock with the recovery phrase, then enroll it.'
      : 'No passkey secret in this session — sign in with the account passkey, or use the recovery phrase.';
  }
  return false;
}

/* Re-wrap a legacy (fleet-derived) prf envelope onto the dedicated vault
   PRF domain after a successful unlock. Quiet best-effort: rides the
   publish chain; a failure just means the envelope migrates on a later
   unlock instead. */
function vaultMigratePrfEnvelope(envelopeId) {
  vaultPublishChain = vaultPublishChain
    .then(async () => {
      if (!vaultKernelToken || !vaultState.blob) return false;
      const [secret, mark] = vaultPrfWrapSource();
      if (!secret || mark !== VAULT_PRF_ENVELOPE_MARK) return false;
      const envelopes = [];
      let migrated = false;
      for (const envelope of vaultState.blob.envelopes || []) {
        if (envelope.kind === 'prf' && envelope.id === envelopeId && !envelope.prf) {
          envelopes.push({
            ...envelope,
            prf: VAULT_PRF_ENVELOPE_MARK,
            ...(await vaultKernelCall('wrap-new-envelope', {
              token: vaultKernelToken,
              prf_secret: secret,
            })),
          });
          migrated = true;
        } else {
          envelopes.push(envelope);
        }
      }
      if (!migrated) return false;
      vaultState.blob = { ...vaultState.blob, envelopes };
      await vaultPersist();
      console.info('[vault] passkey envelope migrated to the dedicated vault PRF domain');
      return true;
    })
    .catch(err => {
      console.warn('[vault] PRF envelope migration deferred:', err?.message || err);
      return false;
    });
}

/* A local WebAuthn assertion purely to (re-)evaluate the PRF extension —
   no server round-trip; the fresh secrets also re-arm fleet-sync. Both
   salts in one gesture: `first` = fleet-sync, `second` = the vault's own
   domain. */
async function vaultRequestPrfSecret() {
  const challenge = crypto.getRandomValues(new Uint8Array(32));
  const credential = await navigator.credentials.get({
    publicKey: {
      challenge,
      userVerification: 'preferred',
      extensions: {
        prf: {
          eval: {
            first: new TextEncoder().encode('intendant-fleet-sync-v1'),
            second: new TextEncoder().encode(VAULT_HKDF_SALT),
          },
        },
      },
    },
  });
  const results = credential?.getClientExtensionResults?.()?.prf?.results;
  const first = results?.first;
  if (!first) return false;
  sessionStorage.setItem(FLEET_PRF_SESSION_KEY, dashboardBytesToBase64Url(new Uint8Array(first)));
  if (results?.second) {
    sessionStorage.setItem(
      VAULT_PRF_SESSION_KEY,
      dashboardBytesToBase64Url(new Uint8Array(results.second))
    );
  }
  accessFleetAesKey = null;
  return true;
}

async function vaultUnlockWithPasskey() {
  vaultState.lastError = '';
  try {
    if (!(await vaultTryPrfUnlock({ silent: true }))) {
      if (await vaultRequestPrfSecret()) {
        await vaultTryPrfUnlock();
      } else {
        vaultState.lastError = 'This authenticator did not return a PRF secret — use the recovery phrase.';
      }
    }
  } catch (err) {
    vaultState.lastError = `Passkey unlock failed: ${err?.message || err}`;
  }
  renderAccessVaultSection();
}

async function vaultUnlockWithPhrase(input) {
  vaultState.lastError = '';
  const phrase = await vaultNormalizePhrase(input);
  if (!phrase) {
    vaultState.lastError = 'That is not a valid 12-word recovery phrase.';
    renderAccessVaultSection();
    return false;
  }
  let result;
  try {
    result = await vaultKernelCall('unlock-phrase', {
      phrase,
      envelopes: vaultState.blob?.envelopes || [],
    });
  } catch (err) {
    if (!err?.vaultKernelIntegrity) {
      vaultState.lastError = `Phrase unlock failed: ${err?.message || err}`;
    }
    renderAccessVaultSection();
    return false;
  }
  // matchedEnvelopeId means "the prf envelope this session's passkey
  // opened" — a phrase unlock matches none, which is exactly what
  // makes the enroll-this-passkey offer appear.
  if (result.unlocked) return vaultFinishUnlock(result.token, null);
  vaultState.lastError = 'The phrase is well-formed but does not open this vault.';
  renderAccessVaultSection();
  return false;
}

async function vaultCreate(phrase) {
  if (!vaultAvailable()) throw new Error('the vault needs an authorized trusted dashboard session');
  const now = Date.now();
  // The page owns the metadata (ids, labels, timestamps); the kernel
  // generates the master key, wraps the envelopes, encrypts the empty
  // body, and MACs the assembled blob — the key never exists here.
  const [prfSecret, prfMark] = vaultPrfWrapSource();
  const { token, blob, matched_envelope_id: matched } = await vaultKernelCall('create', {
    phrase,
    phrase_envelope: {
      kind: 'phrase',
      id: vaultRandomId('env'),
      label: 'Recovery phrase',
      created_unix_ms: now,
    },
    prf_secret: prfSecret,
    prf_mark: prfMark,
    prf_envelope: prfSecret
      ? {
          kind: 'prf',
          id: vaultRandomId('env'),
          label: `Passkey enrolled ${new Date(now).toISOString().slice(0, 10)}`,
          created_unix_ms: now,
        }
      : null,
    revision: Math.max(1, vaultState.highWater + 1),
    now,
  });
  try {
    await vaultServerPublish(blob);
  } catch (err) {
    // Nothing was committed: drop the kernel's key with the ceremony.
    vaultKernelLock();
    throw err;
  }
  vaultState.blob = blob;
  vaultState.revision = blob.revision;
  vaultState.highWater = Math.max(vaultState.highWater, blob.revision);
  vaultState.macSeen = true;
  vaultWriteLocal();
  await vaultFinishUnlock(token, matched);
}

/* Re-encrypt and publish the unlocked state as the next revision. On a
   revision conflict: refetch, merge entries by (id, updated_unix_ms) —
   a concurrent update wins over a concurrent delete, never silently
   dropping a credential — and retry once. */
async function vaultPersist() {
  if (!vaultKernelToken || !vaultState.blob) throw new Error('vault is locked');
  const attempt = async () => {
    const revision = Math.max(vaultState.revision, vaultState.highWater) + 1;
    const blob = {
      ...vaultState.blob,
      revision,
      updated_unix_ms: Date.now(),
      body: await vaultKernelCall('encrypt-body', {
        token: vaultKernelToken,
        body: { entries: vaultState.entries, settings: vaultState.settings },
        revision,
      }),
    };
    // Recompute — the spread carries the previous revision's MAC.
    blob.mac = (await vaultKernelCall('compute-mac', { token: vaultKernelToken, blob })).mac;
    await vaultServerPublish(blob);
    vaultState.blob = blob;
    vaultState.revision = revision;
    vaultState.highWater = Math.max(vaultState.highWater, revision);
    vaultState.macSeen = true;
    vaultWriteLocal();
  };
  try {
    await attempt();
  } catch (err) {
    if (!err?.vaultConflict) throw err;
    const server = await vaultServerFetch();
    if (server?.vault) {
      // Authenticate the refetched blob before merging: adopting a spliced
      // envelope set here and re-signing it in the retry would launder the
      // splice into a valid MAC.
      const remoteAuthentic = server.vault.mac
        ? (await vaultKernelCall('verify-mac', { token: vaultKernelToken, blob: server.vault })).valid
        : !vaultState.macSeen;
      if (!remoteAuthentic) {
        vaultState.lastError = 'The conflict refetch returned a vault blob that failed its integrity check — keeping local state.';
        renderAccessVaultSection();
        throw err;
      }
      if (server.vault.mac) vaultMarkMacSeen();
      const { body: remoteBody } = await vaultKernelCall('decrypt-body', {
        token: vaultKernelToken,
        blob: server.vault,
      });
      if (!remoteBody) {
        vaultLock('The vault was re-keyed on another device — unlock it again.');
        throw err;
      }
      vaultState.blob = server.vault;
      vaultState.revision = Number(server.vault.revision) || vaultState.revision;
      vaultState.highWater = Math.max(vaultState.highWater, vaultState.revision);
      vaultState.entries = vaultMergeEntries(
        Array.isArray(remoteBody.entries) ? remoteBody.entries : [],
        vaultState.entries
      );
      vaultState.settings = { ...(remoteBody.settings || {}), ...vaultState.settings };
    }
    await attempt();
  }
  vaultSyncVoiceMirror();
  renderAccessVaultSection();
}

function vaultMergeEntries(remote, local) {
  const byId = new Map();
  for (const entry of remote) if (entry?.id) byId.set(entry.id, entry);
  for (const entry of local) {
    if (!entry?.id) continue;
    const existing = byId.get(entry.id);
    if (!existing || (Number(entry.updated_unix_ms) || 0) >= (Number(existing.updated_unix_ms) || 0)) {
      byId.set(entry.id, entry);
    }
  }
  return Array.from(byId.values());
}

/* Serialize publishes; resolves true on success, false after reporting. */
function vaultQueuePersist() {
  const result = vaultPublishChain
    .then(async () => {
      await vaultPersist();
      return true;
    })
    .catch(err => {
      vaultState.lastError = `Vault sync failed: ${err?.message || err}`;
      renderAccessVaultSection();
      return false;
    });
  vaultPublishChain = result;
  return result;
}

/* Per-entry unseal policy (docs/src/trust-tiers.md, hook 3). 'any' (or
   absent — every pre-policy entry) uses the entry everywhere the vault
   opens; 'trusted' refuses use from a future hosted client. The shipped
   vault UI is already daemon-origin and uses the daemon store. This remains
   client-side self-enforcement: useful against mistakes, not malicious code
   on whatever origin is allowed to unseal. The policy rides inside the
   encrypted body like every other entry field. */
function vaultEntryUnsealPolicy(entry) {
  return entry?.unseal_policy === 'trusted' ? 'trusted' : 'any';
}

function vaultEntryUsableHere(entry) {
  return vaultEntryUnsealPolicy(entry) !== 'trusted' || !DASHBOARD_CONNECT_MODE;
}

function vaultUpsertEntry(entry) {
  const now = Date.now();
  const existing = entry.id ? vaultState.entries.find(e => e.id === entry.id) : null;
  if (existing) {
    Object.assign(existing, entry, { updated_unix_ms: now });
  } else {
    vaultState.entries.push({
      ...entry,
      id: entry.id || vaultRandomId('cred'),
      created_unix_ms: now,
      updated_unix_ms: now,
    });
  }
  vaultSyncVoiceMirror();
  return vaultQueuePersist();
}

function vaultRemoveEntry(id) {
  vaultState.entries = vaultState.entries.filter(e => e.id !== id);
  vaultRevealedEntries.delete(id);
  vaultSyncVoiceMirror();
  return vaultQueuePersist();
}

/* ── Write-only deposits (the CLI lane) ──
   `intendant vault deposit <label>` seals a secret to the vault's deposit
   public key and queues it on the DAEMON (vault_deposits.rs) — the daemon
   holds ciphertext only, and the plaintext never rides a web UI. After
   every unlock we (1) ensure the deposit keypair exists inside the sealed
   body's settings and publish its public half to this daemon, then
   (2) fold queued deposits into the vault as ordinary entries, consuming
   them only AFTER the re-wrapped blob has published. The crypto (P-256
   ECDH → HKDF-SHA256 → AES-256-GCM, label-bound) lives in the kernel
   (open-deposit / generate-deposit-keypair) and mirrors vault_deposits.rs
   (v1) exactly. Cross-implementation parity harness:
   scripts/vault-deposit-parity.cjs. */

async function vaultDepositLaneSync() {
  if (vaultState.status !== 'unlocked' || !vaultKernelToken) return;
  try {
    // 1. Keypair lives inside the sealed body (extractable so it rides
    //    the blob to every unlocking device; it exists only as ciphertext
    //    at rest). The kernel generates it; the private JWK is body
    //    material by design — it must reach every unlocking device.
    let lane = vaultState.settings.deposit_lane;
    if (!lane || !lane.priv_jwk || !lane.pub_raw_b64u) {
      const pair = await vaultKernelCall('generate-deposit-keypair', { token: vaultKernelToken });
      lane = {
        alg: 'ECDH-P256',
        priv_jwk: pair.priv_jwk,
        pub_raw_b64u: pair.pub_raw_b64u,
        created_unix_ms: Date.now(),
      };
      vaultState.settings.deposit_lane = lane;
      await vaultQueuePersist();
    }

    // 2. Publish the public half to this daemon (idempotent; a daemon can
    //    hold at most one deposit key — last unlocked vault wins, and the
    //    mismatch check keeps this a no-op in the steady state).
    const current = await vaultLeaseRpc('api_daemon_vault_deposit_key_fetch').catch(() => null);
    if (!current?.present || current.pub_raw_b64u !== lane.pub_raw_b64u) {
      await vaultLeaseRpc('api_daemon_vault_deposit_key_publish', {
        alg: 'ECDH-P256',
        pub_raw_b64u: lane.pub_raw_b64u,
      });
    }

    // 3. Fold queued deposits, then consume. Consume strictly after the
    //    folded blob PUBLISHED — a failed publish leaves them queued.
    const fetched = await vaultLeaseRpc('api_daemon_vault_deposits_fetch').catch(() => null);
    const deposits = Array.isArray(fetched?.deposits) ? fetched.deposits : [];
    if (!deposits.length) return;
    const consumed = [];
    for (const dep of deposits) {
      try {
        const { secret } = await vaultKernelCall('open-deposit', {
          token: vaultKernelToken,
          deposit: dep,
          lane_priv_jwk: lane.priv_jwk,
          lane_pub_raw_b64u: lane.pub_raw_b64u,
        });
        vaultState.entries.push({
          id: vaultRandomId('cred'),
          kind: 'api_key',
          provider: '',
          label: String(dep.label || 'CLI deposit'),
          secret,
          origin: 'cli-deposit',
          created_unix_ms: Number(dep.created_unix_ms) || Date.now(),
          updated_unix_ms: Date.now(),
        });
        consumed.push(String(dep.id));
      } catch (err) {
        // Sealed to a superseded deposit key, or corrupt: leave it queued
        // (visible via `intendant vault status`); never consume blind.
        console.warn('[vault] deposit', dep?.id, 'did not open — leaving it queued:', err?.message || err);
      }
    }
    if (!consumed.length) return;
    await vaultQueuePersist();
    await vaultLeaseRpc('api_daemon_vault_deposits_consume', { ids: consumed });
    vaultSyncVoiceMirror();
    renderAccessVaultSection();
    console.info(`[vault] folded ${consumed.length} CLI deposit(s) into the vault`);
  } catch (err) {
    // Advisory lane: never let it break an unlock.
    console.warn('[vault] deposit-lane sync failed:', err?.message || err);
  }
}

async function vaultEnrollThisPasskey() {
  if (!vaultKernelToken) return;
  vaultState.lastError = '';
  try {
    let [secret, mark] = vaultPrfWrapSource();
    if (!secret) {
      if (!(await vaultRequestPrfSecret())) throw new Error('this authenticator did not return a PRF secret');
      [secret, mark] = vaultPrfWrapSource();
    }
    if (!secret) throw new Error('no PRF secret available');
    // Already enrolled? The kernel probes which envelope (if any) this
    // session's secrets open, without touching the held key.
    const { envelope_id: matched } = await vaultKernelCall('match-prf-envelope', {
      envelopes: vaultState.blob.envelopes || [],
      secret_dedicated: vaultPrfSecretDedicated(),
      secret_legacy: vaultPrfSecretLegacy(),
    });
    if (matched) {
      vaultState.matchedEnvelopeId = matched;
      renderAccessVaultSection();
      return;
    }
    const now = Date.now();
    const envelope = {
      kind: 'prf',
      id: vaultRandomId('env'),
      label: `Passkey enrolled ${new Date(now).toISOString().slice(0, 10)}`,
      created_unix_ms: now,
      ...(mark ? { prf: mark } : {}),
      ...(await vaultKernelCall('wrap-new-envelope', {
        token: vaultKernelToken,
        prf_secret: secret,
      })),
    };
    vaultState.blob = { ...vaultState.blob, envelopes: [...(vaultState.blob.envelopes || []), envelope] };
    vaultState.matchedEnvelopeId = envelope.id;
    await vaultQueuePersist();
  } catch (err) {
    vaultState.lastError = `Passkey enrollment failed: ${err?.message || err}`;
    renderAccessVaultSection();
  }
}

/* ── Voice keys: vault-first, localStorage fallback ── */

const VAULT_VOICE_STORAGE_PROVIDERS = { gemini_api_key: 'gemini', openai_api_key: 'openai' };
let vaultVoiceMirror = {};

/* Synchronous mirror of the voice-relevant entries, so the existing
   synchronous voice paths keep working without an unlock await. */
function vaultSyncVoiceMirror() {
  const mirror = {};
  if (vaultState.status === 'unlocked' || vaultKernelToken) {
    const usable = vaultState.entries.filter(vaultEntryUsableHere);
    for (const [storageKey, provider] of Object.entries(VAULT_VOICE_STORAGE_PROVIDERS)) {
      const entry =
        usable.find(e => e.kind === 'api_key' && e.provider === provider && e.voice && e.secret) ||
        usable.find(e => e.kind === 'api_key' && e.provider === provider && e.secret);
      if (entry) mirror[storageKey] = String(entry.secret);
    }
  }
  vaultVoiceMirror = mirror;
}

function voiceApiKeyGet(storageKey = getStorageKey()) {
  return vaultVoiceMirror[storageKey] || localStorage.getItem(storageKey) || '';
}

function voiceApiKeySet(key, storageKey = getStorageKey()) {
  const value = String(key || '').trim();
  if (!value) return;
  const provider = VAULT_VOICE_STORAGE_PROVIDERS[storageKey];
  if (provider && vaultState.status === 'unlocked') {
    vaultVoiceMirror[storageKey] = value;
    const existing = vaultState.entries.find(e => e.kind === 'api_key' && e.provider === provider && e.voice);
    vaultUpsertEntry(
      existing
        ? { ...existing, secret: value }
        : {
            kind: 'api_key',
            provider,
            voice: true,
            label: `${provider === 'openai' ? 'OpenAI' : 'Gemini'} voice key`,
            secret: value,
          }
    ).then(ok => {
      // Keep the key reachable even if the publish failed.
      if (ok) localStorage.removeItem(storageKey);
      else localStorage.setItem(storageKey, value);
    });
    return;
  }
  localStorage.setItem(storageKey, value);
}

/* One-time migration: move today's per-origin localStorage voice keys
   into the vault. The local copy is removed only after a successful
   publish, so a failed sync can never lose a key. */
async function vaultMigrateVoiceKeys() {
  if (vaultState.status !== 'unlocked') return;
  const migrated = [];
  for (const [storageKey, provider] of Object.entries(VAULT_VOICE_STORAGE_PROVIDERS)) {
    const value = String(localStorage.getItem(storageKey) || '').trim();
    if (!value) continue;
    const existing = vaultState.entries.find(e => e.kind === 'api_key' && e.provider === provider && e.voice);
    if (existing) {
      // Same secret: the vault already owns it. Different: the vault
      // copy wins; keep the local one for manual reconciliation.
      if (existing.secret === value) localStorage.removeItem(storageKey);
      continue;
    }
    const now = Date.now();
    vaultState.entries.push({
      id: vaultRandomId('cred'),
      kind: 'api_key',
      provider,
      voice: true,
      label: `${provider === 'openai' ? 'OpenAI' : 'Gemini'} voice key (migrated)`,
      secret: value,
      created_unix_ms: now,
      updated_unix_ms: now,
    });
    migrated.push(storageKey);
  }
  if (!migrated.length) return;
  vaultSyncVoiceMirror();
  if (!(await vaultQueuePersist())) return;
  for (const storageKey of migrated) localStorage.removeItem(storageKey);
  vaultState.migratedVoiceKeys = true;
  renderAccessVaultSection();
}

/* ── Fueling: vault entries → credential leases on this daemon ──
   The connected daemon borrows credentials over the verified tunnel:
   grants carry the material + TTL + offline window, this tab renews
   every 5 minutes while attached, and the offline knob decides how long
   the daemon keeps working after the last fueling session detaches. */

const VAULT_LEASE_TTL_MS = 15 * 60 * 1000;
const VAULT_LEASE_RENEW_EVERY_MS = 5 * 60 * 1000;
const VAULT_OFFLINE_CHOICES = [
  [0, 'while connected only'],
  [60 * 60 * 1000, '1 hour offline'],
  [24 * 60 * 60 * 1000, '24 hours offline'],
  [72 * 60 * 60 * 1000, '3 days offline'],
];
const VAULT_OFFLINE_DEFAULT_MS = 24 * 60 * 60 * 1000;

const vaultLeaseState = {
  supported: null,   // null until first verdict; false only on the daemon's own refusal
  availability: '',  // honest cause: 'denied' | 'unsupported' | 'connected' (transport F6)
  leases: [],
  egress: [],        // active client-egress relays (the path indicator)
  expiredNote: '',
  lastError: '',
  fetchedAt: 0,
  busy: false,
};
const vaultOwnLeaseIds = new Set(); // leases granted by this tab — the ones we renew
let vaultRenewTimer = null;

function vaultLeaseDaemonKey(suffix) {
  return `intendant_vault_${suffix}_${DASHBOARD_CONNECT_DAEMON_ID || 'local'}`;
}

function vaultOfflineMs() {
  const raw = Number(localStorage.getItem(vaultLeaseDaemonKey('offline_ms')));
  return Number.isFinite(raw) && raw >= 0 ? raw : VAULT_OFFLINE_DEFAULT_MS;
}

function vaultSetOfflineMs(value) {
  localStorage.setItem(vaultLeaseDaemonKey('offline_ms'), String(value));
}

/* Full-credential OAuth leases are an explicit per-daemon opt-in (the
   sign-off default is OFF): leasing a pasted auth-file JSON hands the
   daemon durable authority for the lease window. */
function vaultOauthLeasesEnabled() {
  return localStorage.getItem(vaultLeaseDaemonKey('oauth_leases')) === 'true';
}

function vaultSetOauthLeasesEnabled(enabled) {
  localStorage.setItem(vaultLeaseDaemonKey('oauth_leases'), enabled ? 'true' : 'false');
}

/* ── Access-token OAuth mode (the default) ──
   The vault entry holds the full auth file, but what leaves this browser
   by default is only a short-lived access token: the refresh token stays
   in the vault, this tab performs the provider refresh itself (writing
   rotated tokens back into the vault), and the grant carries
   mode:'access_token' — material the daemon verifies is refresh-free
   before accepting. The full-credential opt-in above remains for long
   unattended autonomy and for providers whose token endpoint refuses
   browser CORS (Anthropic's currently does; OpenAI's allows any origin). */

const VAULT_OAUTH_PROVIDERS = {
  'oauth:codex': {
    tokenUrl: 'https://auth.openai.com/oauth/token',
    // The Codex CLI's public OAuth client id (a PKCE public client, not a secret).
    clientId: 'app_EMoamEEZ73f0CkXaXp7hrann',
    scope: 'openid profile email',
  },
  'oauth:claude-code': {
    tokenUrl: 'https://console.anthropic.com/v1/oauth/token',
    // Claude Code's public OAuth client id.
    clientId: '9d1c250a-e61b-44d9-88ed-5944d1962f5e',
    corsNote:
      'Anthropic’s token endpoint refuses browser origins, so this tab cannot refresh Claude Code tokens — enable full-credential OAuth leases to fuel it.',
  },
};
/* Refresh when the current token has less life left than this. With
   provider access tokens living ≈1 h against a 5-minute renewal tick,
   the daemon-side token is always at least this fresh. */
const VAULT_OAUTH_REFRESH_MARGIN_MS = 10 * 60 * 1000;
const vaultOauthEndpointOverrides = {}; // validator-only (debug handle)

function vaultOauthRefreshTokenOf(kind, secretJson) {
  if (kind === 'oauth:codex') return String(secretJson?.tokens?.refresh_token || '');
  if (kind === 'oauth:claude-code') return String(secretJson?.claudeAiOauth?.refreshToken || '');
  return '';
}

/* Epoch ms when the entry's current access token dies; 0 = unknown,
   which reads as already-stale so the first fueling always refreshes. */
function vaultOauthExpiryMs(kind, secretJson) {
  if (kind === 'oauth:claude-code') return Number(secretJson?.claudeAiOauth?.expiresAt) || 0;
  if (kind === 'oauth:codex') {
    try {
      // ChatGPT-plan access tokens are JWTs; exp is authoritative and
      // survives page reloads (unlike a refresh-time bookkeeping map).
      const payload = String(secretJson.tokens.access_token).split('.')[1];
      const claims = JSON.parse(atob(payload.replace(/-/g, '+').replace(/_/g, '/')));
      return (Number(claims.exp) || 0) * 1000;
    } catch {
      return 0;
    }
  }
  return 0;
}

/* The lease material for access-token mode: the auth file with every
   durable field blanked. Empty strings rather than deletions — the
   agents' deserializers expect the fields — and the daemon re-verifies
   the result before accepting the grant. */
function vaultOauthAccessMaterial(kind, secretJson) {
  const copy = JSON.parse(JSON.stringify(secretJson));
  if (kind === 'oauth:codex') {
    if (copy.tokens) copy.tokens.refresh_token = '';
    if (typeof copy.OPENAI_API_KEY === 'string') copy.OPENAI_API_KEY = null;
  }
  if (kind === 'oauth:claude-code' && copy.claudeAiOauth) copy.claudeAiOauth.refreshToken = '';
  return JSON.stringify(copy);
}

/* Provider refresh from this tab. Writes back into the vault entry: the
   new access token always, the rotated refresh token when the provider
   rotates (both do) — without the write-back the stored refresh token
   could be single-use and dead by the next unlock. */
async function vaultOauthRefresh(kind, entry) {
  const provider = VAULT_OAUTH_PROVIDERS[kind];
  if (!provider) throw new Error(`no OAuth refresh route for ${kind}`);
  let secretJson;
  try {
    secretJson = JSON.parse(String(entry.secret));
  } catch {
    throw new Error('the stored auth JSON does not parse — re-paste the auth file');
  }
  const refreshToken = vaultOauthRefreshTokenOf(kind, secretJson);
  if (!refreshToken) throw new Error('the stored auth JSON has no refresh token — re-paste the auth file');
  let response;
  try {
    response = await fetch(vaultOauthEndpointOverrides[kind] || provider.tokenUrl, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        grant_type: 'refresh_token',
        client_id: provider.clientId,
        refresh_token: refreshToken,
        ...(provider.scope ? { scope: provider.scope } : {}),
      }),
    });
  } catch (err) {
    throw new Error(
      `token refresh unreachable (${err?.message || err})${provider.corsNote ? ` — ${provider.corsNote}` : ''}`
    );
  }
  if (!response.ok) throw new Error(`token refresh failed: HTTP ${response.status}`);
  const fresh = await response.json();
  if (!fresh?.access_token) throw new Error('token refresh returned no access token');
  const nowMs = Date.now();
  if (kind === 'oauth:codex') {
    secretJson.tokens = secretJson.tokens || {};
    secretJson.tokens.access_token = String(fresh.access_token);
    if (fresh.id_token) secretJson.tokens.id_token = String(fresh.id_token);
    if (fresh.refresh_token) secretJson.tokens.refresh_token = String(fresh.refresh_token);
    secretJson.last_refresh = new Date(nowMs).toISOString();
  } else if (kind === 'oauth:claude-code') {
    secretJson.claudeAiOauth = secretJson.claudeAiOauth || {};
    secretJson.claudeAiOauth.accessToken = String(fresh.access_token);
    if (fresh.refresh_token) secretJson.claudeAiOauth.refreshToken = String(fresh.refresh_token);
    if (fresh.expires_in) secretJson.claudeAiOauth.expiresAt = nowMs + Number(fresh.expires_in) * 1000;
  }
  vaultUpsertEntry({ id: entry.id, secret: JSON.stringify(secretJson) });
  return secretJson;
}

/* Fresh access-token material for an oauth entry, refreshing through the
   provider first when the current token is inside the margin. */
async function vaultOauthAccessTokenMaterial(entry, kind) {
  if (vaultState.status !== 'unlocked') {
    throw new Error('unlock the vault to refresh the access token');
  }
  let secretJson;
  try {
    secretJson = JSON.parse(String(entry.secret));
  } catch {
    throw new Error('the stored auth JSON does not parse — re-paste the auth file');
  }
  if (vaultOauthExpiryMs(kind, secretJson) - Date.now() < VAULT_OAUTH_REFRESH_MARGIN_MS) {
    secretJson = await vaultOauthRefresh(kind, entry);
  }
  return vaultOauthAccessMaterial(kind, secretJson);
}

/* Availability of the lease RPC family here (transport F6, design §3.4):
   the daemon's own word, before any RPC fires. Reasons the render paths
   branch on — 'denied' (the status boolean says this session's role is
   refused credentials.manage; custody methods have no runtime-ready
   ladder, so false means exactly that), 'unsupported' (the hello_ack
   features list omits the family — the daemon predates it),
   'transport-down' (no verified tunnel; custody has no HTTP twin by
   design, so no other lane can answer), 'connected' (go — optimistically
   so while the handshake is still digesting, letting the RPCs answer
   exactly as the pre-facade probes did). */
function vaultLeaseAvailability() {
  return daemonApi.availability('api_credential_lease_status');
}

function vaultLeaseTransportReady() {
  return vaultLeaseAvailability().ok;
}

/* One custody RPC. Tunnel-only by design (docs/src/credential-custody.md;
   the transport design keeps the family off HTTP rows), which the facade
   enforces: no fallback lane exists, so a custody mutation can never
   replay elsewhere after an ambiguous failure, and a down tunnel rejects
   immediately. Resolves with the result payload (the pre-facade contract
   every call site and window.intendantVault.probeEgress keeps);
   rejections are DaemonApiError, whose `kind` the refreshers classify
   into availability verdicts. Per-method timeouts live in
   dashboardControlRequestTimeoutMs — the `{ timeoutMs: 15000 }` option
   the pre-facade call passed here was silently ignored by the request
   verb; the custody rows of that table now carry the intent. */
async function vaultLeaseRpc(method, params = {}) {
  const { body } = await daemonApi.request(method, params);
  return body;
}

/* Whether the tunneled daemon offers local vault storage to THIS session —
   'connected' only: the daemon must have confirmed the method (status
   boolean or features list), because this gates an optional action (the
   sealed-copy button) that a credentials.manage-denied session or an
   older daemon could only bounce. */
function vaultTunnelDaemonVaultAvailable() {
  return daemonApi.availability('api_daemon_vault_publish').reason === 'connected';
}

/* The lease kind a vault entry fuels, or null when it cannot fuel.
   A trusted-only entry cannot fuel from this context at all — this is
   the single choke point for fueling, re-fueling, and renew re-grants. */
function vaultEntryLeaseKind(entry) {
  if (!entry || !entry.secret) return null;
  if (!vaultEntryUsableHere(entry)) return null;
  if (entry.kind === 'api_key' && ['anthropic', 'openai', 'gemini'].includes(entry.provider)) {
    return `api_key:${entry.provider}`;
  }
  if (entry.kind === 'oauth' && ['codex', 'claude-code'].includes(entry.provider)) {
    return `oauth:${entry.provider}`;
  }
  return null;
}

async function vaultRefreshLeases({ force = false } = {}) {
  const avail = vaultLeaseAvailability();
  if (!avail.ok) {
    // The daemon already answered through features/status — record the
    // verdict without firing RPCs that can only bounce (the render paths
    // read the same availability live, so no re-render is needed here).
    if (avail.reason === 'denied' || avail.reason === 'unsupported') {
      vaultLeaseState.supported = false;
      vaultLeaseState.availability = avail.reason;
    }
    return;
  }
  if (!force && Date.now() - vaultLeaseState.fetchedAt < 30_000) return;
  try {
    const result = await vaultLeaseRpc('api_credential_lease_status');
    vaultLeaseState.supported = true;
    vaultLeaseState.availability = 'connected';
    vaultLeaseState.leases = Array.isArray(result?.leases) ? result.leases : [];
    vaultLeaseState.egress = Array.isArray(result?.egress) ? result.egress : [];
    vaultLeaseState.expiredNote = String(result?.expired_note || '');
    vaultLeaseState.lastError = '';
    vaultLeaseState.fetchedAt = Date.now();
    for (const id of Array.from(vaultOwnLeaseIds)) {
      if (!vaultLeaseState.leases.some(lease => lease.lease_id === id)) vaultOwnLeaseIds.delete(id);
    }
    vaultEgressEnsure().catch(() => {});
  } catch (err) {
    vaultLeaseState.lastError = String(err?.message || err);
    vaultLeaseState.fetchedAt = Date.now();
    // Availability honesty (transport F6): only the daemon's own refusals
    // are verdicts about this session or this daemon. 'denied' is the
    // authorizer refusing the gate, 'unavailable' is a daemon without the
    // method (races where the pre-flight was still optimistic). A timeout
    // or a dropped channel says nothing about support — keep the last
    // known verdict and surface the error text instead of the old
    // conflated "cannot manage leases" reading.
    const kind = err?.kind || '';
    if (kind === 'denied' || kind === 'unavailable') {
      vaultLeaseState.supported = false;
      vaultLeaseState.availability = kind === 'denied' ? 'denied' : 'unsupported';
    }
  }
  renderAccessVaultSection();
  refreshUnfueledEmptyState().catch(() => {});
  vaultRefreshCustody().catch(() => {});
}

/* Custody trail: the daemon's own record of lease/relay lifecycle events,
   fetched over the same credentials.manage-gated channel as the leases.
   `availability` carries the honest cause when supported goes false:
   'denied' (role refused the gate) vs 'unsupported' (daemon predates the
   trail) — transient lane failures flip neither. */
const custodyTrailState = { events: [], supported: null, availability: '', fetchedAt: 0 };

async function vaultRefreshCustody({ force = false } = {}) {
  const avail = daemonApi.availability('api_credential_custody_trail');
  if (!avail.ok) {
    if (avail.reason === 'denied' || avail.reason === 'unsupported') {
      const changed = custodyTrailState.supported !== false
        || custodyTrailState.availability !== avail.reason;
      custodyTrailState.supported = false;
      custodyTrailState.availability = avail.reason;
      custodyTrailState.fetchedAt = Date.now();
      if (changed) renderAccessCustodySection();
    }
    return;
  }
  if (!force && Date.now() - custodyTrailState.fetchedAt < 30_000) return;
  custodyTrailState.fetchedAt = Date.now();
  try {
    const result = await vaultLeaseRpc('api_credential_custody_trail');
    custodyTrailState.events = Array.isArray(result?.events) ? result.events : [];
    custodyTrailState.supported = true;
    custodyTrailState.availability = 'connected';
  } catch (err) {
    // Verdicts only from the daemon's own refusals (see
    // vaultRefreshLeases); a timeout or dropped channel keeps whatever
    // the last connected read established.
    const kind = err?.kind || '';
    if (kind === 'denied' || kind === 'unavailable') {
      custodyTrailState.supported = false;
      custodyTrailState.availability = kind === 'denied' ? 'denied' : 'unsupported';
    }
  }
  renderAccessCustodySection();
}

const CUSTODY_EVENT_VIEW = {
  lease_granted: { chip: 'granted', cls: 'grant' },
  lease_revoked: { chip: 'revoked', cls: 'revoke' },
  lease_expired: { chip: 'expired', cls: 'expire' },
  egress_registered: { chip: 'relay on', cls: 'egress' },
  egress_unregistered: { chip: 'relay off', cls: 'egress' },
  custody_reset: { chip: 'reset', cls: 'reset' },
};

function renderAccessCustodySection() {
  const mount = document.getElementById('access-custody-section');
  if (!mount) return;
  mount.innerHTML = '';
  const note = text => {
    const el = document.createElement('div');
    el.className = 'vault-note';
    el.textContent = text;
    mount.appendChild(el);
  };
  if (custodyTrailState.supported === false) {
    // The honest split (transport F6): the daemon said which it is.
    note(custodyTrailState.availability === 'unsupported'
      ? 'This daemon predates the custody trail — upgrade it to see lease and relay lifecycle events.'
      : "This session can't read the custody trail — its role needs credentials.manage.");
    return;
  }
  if (!custodyTrailState.events.length) {
    note('No custody events yet — lease grants, expiries, revocations, and relay changes will appear here.');
    return;
  }
  const list = document.createElement('div');
  list.className = 'custody-trail';
  const shown = custodyTrailState.events.slice(0, 50);
  for (const event of shown) {
    const row = document.createElement('div');
    row.className = 'custody-row';
    const ts = document.createElement('span');
    ts.className = 'custody-ts';
    ts.textContent = formatLogTimestampLabel(event.at_unix_ms);
    const view = CUSTODY_EVENT_VIEW[event.event] || { chip: event.event, cls: '' };
    const chip = document.createElement('span');
    chip.className = 'custody-chip' + (view.cls ? ' ' + view.cls : '');
    chip.textContent = view.chip;
    const main = document.createElement('span');
    main.className = 'custody-main';
    main.textContent = [event.label, event.kind].filter(Boolean).join(' · ');
    const meta = document.createElement('span');
    meta.className = 'custody-meta';
    meta.textContent = [
      event.actor ? `by ${event.actor}` : '',
      event.origin ? `via ${event.origin}` : '',
      event.detail,
    ]
      .filter(Boolean)
      .join(' — ');
    row.append(ts, chip, main, meta);
    list.appendChild(row);
  }
  mount.appendChild(list);
  if (custodyTrailState.events.length > shown.length) {
    note(`…and ${custodyTrailState.events.length - shown.length} older events on the daemon.`);
  }
}

/* ── Agent account sign-in ceremonies ──
   The guided sign-in cards (/api/claude-auth/* + /api/codex-auth/*):
   the daemon runs each CLI's own login ceremony on a private PTY; a
   card walks the owner through it. Claude: open the sign-in URL in
   THIS browser, paste the code Anthropic shows back here. Codex: open
   the verification URL, type the one-time code the card shows into
   OpenAI's page — nothing comes back to the daemon. Token exchanges
   stay inside the CLIs on the daemon; this page only ever sees the
   sign-in URL (and, for Codex, the one-time code the owner must read).
   Custody-gated on the daemon (credentials.manage + hosted-provenance
   + lease/egress tier refusals) — the cards render whatever refusal
   the daemon states. One credential ceremony runs at a time across
   both providers (the daemon's status reports `busy_with`). */
const AGENT_SIGNIN_PROVIDERS = {
  claude: {
    label: 'Claude',
    statusMethod: 'api_claude_auth_status',
    startMethod: 'api_claude_auth_start',
    cancelMethod: 'api_claude_auth_cancel',
    codeMethod: 'api_claude_auth_code',
    startParams: { mode: 'claudeai' },
    blastCopy:
      'This changes the Claude account for every Claude Code session on this machine. ' +
      'Running sessions keep the old account until reloaded.',
    startLabel: 'Start Claude sign-in',
    openLabel: 'Open Anthropic sign-in',
    checkNote: 'Check that your browser shows claude.com before signing in.',
    unsupportedNote:
      'This daemon predates the Claude sign-in ceremony — upgrade it to sign in from here.',
    backendMatch: backend => backend.includes('claude'),
    sessionsTitle: 'Running Claude Code sessions',
    noSessionsNote: 'No running Claude Code sessions — new sessions start on the new account.',
    sessionKind: 'claude-code',
    lines: {
      idle: 'Sign this daemon into a Claude account (claude.ai subscription sign-in).',
      starting: 'Starting the Claude CLI’s sign-in…',
      awaiting_browser: 'Open the sign-in link, approve access, then paste the code back here.',
      awaiting_code: 'Open the sign-in link, approve access, then paste the code back here.',
      verifying: 'Verifying with Anthropic… the CLI is exchanging the code.',
      success: 'This machine’s Claude Code now uses the new account.',
    },
  },
  codex: {
    label: 'Codex',
    statusMethod: 'api_codex_auth_status',
    startMethod: 'api_codex_auth_start',
    cancelMethod: 'api_codex_auth_cancel',
    codeMethod: null, // device flow: the code goes to OpenAI's page, never back here
    startParams: { mode: 'chatgpt' },
    blastCopy:
      "This signs this machine's Codex into a ChatGPT account. Every Codex session on " +
      'this machine uses it. Running sessions keep the old account until reloaded.',
    startLabel: 'Start Codex sign-in',
    openLabel: 'Open OpenAI sign-in',
    checkNote: 'Check that your browser shows auth.openai.com before signing in.',
    unsupportedNote:
      'This daemon predates the Codex sign-in ceremony — upgrade it to sign in from here.',
    backendMatch: backend => backend.includes('codex'),
    sessionsTitle: 'Running Codex sessions',
    noSessionsNote: 'No running Codex sessions — new sessions start on the new account.',
    sessionKind: 'codex',
    /* OpenAI's own device-flow warning, kept near-verbatim. */
    deviceWarning:
      'Continue only if you started this login in Codex. If a website or another person ' +
      'gave you this code, cancel.',
    lines: {
      idle: 'Sign this daemon into a ChatGPT account (device sign-in for Codex).',
      starting: 'Starting the Codex CLI’s sign-in…',
      awaiting_user: 'Open the link, sign in, then type the one-time code below into OpenAI’s page.',
      verifying: 'Confirming the sign-in with OpenAI…',
      success: 'This machine’s Codex now uses the new account.',
    },
  },
};

function agentSigninProviderState() {
  return {
    status: null, // last daemon status payload ({phase, url?, user_code?, account?, error?…})
    fetchedAt: 0,
    busy: false,
    lastError: '',
    pollTimer: null,
    sessions: [], // running sessions of this provider's backend for the reload panel
    sessionsFetchedAt: 0,
    reloadRequested: new Set(),
  };
}
const agentSigninState = {
  claude: agentSigninProviderState(),
  codex: agentSigninProviderState(),
};

function agentSigninPhase(provider) {
  return String(agentSigninState[provider].status?.phase || 'idle');
}

function agentSigninActive(provider) {
  return [
    'starting',
    'awaiting_browser',
    'awaiting_code',
    'awaiting_user',
    'verifying',
  ].includes(agentSigninPhase(provider));
}

async function agentSigninRefresh(provider, { force = false } = {}) {
  const spec = AGENT_SIGNIN_PROVIDERS[provider];
  const state = agentSigninState[provider];
  const avail = daemonApi.availability(spec.statusMethod);
  if (avail.reason === 'denied' || avail.reason === 'unsupported') return;
  const freshFor = agentSigninActive(provider) ? 1500 : 15000;
  if (!force && Date.now() - state.fetchedAt < freshFor) return;
  state.fetchedAt = Date.now();
  try {
    const resp = await daemonApi.request(spec.statusMethod, {});
    if (resp.ok) {
      state.status = resp.body;
      state.lastError = '';
    } else {
      state.lastError = String(resp.body?.error || `status ${resp.status}`);
    }
  } catch (err) {
    state.lastError = String(err?.message || err);
  }
  agentSigninEnsurePoll(provider);
  renderAgentSigninSection();
  if (agentSigninPhase(provider) === 'success') {
    agentSigninRefreshSessions(provider).catch(() => {});
  }
}

/* 2s poll while a ceremony is in flight and the page is looking at it;
   self-clearing on terminal/idle phases (the render-time refresh with a
   15s freshness guard covers the rest). */
function agentSigninEnsurePoll(provider) {
  const state = agentSigninState[provider];
  if (agentSigninActive(provider)) {
    if (state.pollTimer) return;
    state.pollTimer = window.setInterval(() => {
      if (document.visibilityState !== 'visible') return;
      const watching =
        typeof paneIsVisible !== 'function' ||
        paneIsVisible('vault') ||
        paneIsVisible('access');
      if (!watching) return;
      agentSigninRefresh(provider, { force: true }).catch(() => {});
    }, 2000);
  } else if (state.pollTimer) {
    window.clearInterval(state.pollTimer);
    state.pollTimer = null;
  }
}

/* 1s countdown ticker for the codex one-time code's expiry: updates the
   countdown text in place (never a full re-render per tick). */
let agentSigninTickTimer = null;
function agentSigninCountdownText(deadlineMs) {
  const left = deadlineMs - Date.now();
  if (!Number.isFinite(left) || left <= 0) return 'code expired';
  const totalSec = Math.floor(left / 1000);
  const minutes = Math.floor(totalSec / 60);
  const seconds = String(totalSec % 60).padStart(2, '0');
  return `code expires in ${minutes}:${seconds}`;
}
function agentSigninEnsureTicker() {
  const needsTick = agentSigninPhase('codex') === 'awaiting_user';
  if (needsTick && !agentSigninTickTimer) {
    agentSigninTickTimer = window.setInterval(() => {
      for (const el of document.querySelectorAll('[data-signin-countdown]')) {
        el.textContent = agentSigninCountdownText(Number(el.dataset.signinCountdown));
      }
    }, 1000);
  } else if (!needsTick && agentSigninTickTimer) {
    window.clearInterval(agentSigninTickTimer);
    agentSigninTickTimer = null;
  }
}

async function agentSigninStart(provider) {
  const spec = AGENT_SIGNIN_PROVIDERS[provider];
  const state = agentSigninState[provider];
  if (state.busy) return;
  state.busy = true;
  state.lastError = '';
  renderAgentSigninSection();
  try {
    const resp = await daemonApi.request(spec.startMethod, { ...spec.startParams });
    if (resp.ok) {
      state.status = resp.body?.status || state.status;
    } else {
      state.lastError = String(resp.body?.error || `start failed (${resp.status})`);
      if (resp.status === 409) await agentSigninRefresh(provider, { force: true });
    }
  } catch (err) {
    state.lastError = String(err?.message || err);
  } finally {
    state.busy = false;
    agentSigninEnsurePoll(provider);
    renderAgentSigninSection();
  }
}

async function agentSigninSubmitCode(provider, code) {
  const spec = AGENT_SIGNIN_PROVIDERS[provider];
  const state = agentSigninState[provider];
  if (!spec.codeMethod || state.busy) return;
  state.busy = true;
  state.lastError = '';
  renderAgentSigninSection();
  try {
    const resp = await daemonApi.request(spec.codeMethod, { code });
    if (resp.ok) {
      if (state.status) state.status.phase = 'verifying';
    } else {
      state.lastError = String(resp.body?.error || `code refused (${resp.status})`);
      if (resp.status === 409) await agentSigninRefresh(provider, { force: true });
    }
  } catch (err) {
    state.lastError = String(err?.message || err);
  } finally {
    state.busy = false;
    agentSigninEnsurePoll(provider);
    renderAgentSigninSection();
  }
}

async function agentSigninCancel(provider) {
  const spec = AGENT_SIGNIN_PROVIDERS[provider];
  const state = agentSigninState[provider];
  state.lastError = '';
  try {
    const resp = await daemonApi.request(spec.cancelMethod, {});
    if (!resp.ok) {
      state.lastError = String(resp.body?.error || `cancel failed (${resp.status})`);
    }
  } catch (err) {
    state.lastError = String(err?.message || err);
  }
  await agentSigninRefresh(provider, { force: true }).catch(() => {});
  renderAgentSigninSection();
}

/* The reload panel's corpus: this provider's live sessions on this
   daemon. The daemon is the authority on reloadability — this list only
   decides which chips to offer. */
async function agentSigninRefreshSessions(provider, { force = false } = {}) {
  const spec = AGENT_SIGNIN_PROVIDERS[provider];
  const state = agentSigninState[provider];
  if (!force && Date.now() - state.sessionsFetchedAt < 10000) return;
  state.sessionsFetchedAt = Date.now();
  try {
    const resp = await daemonApi.request('api_sessions', { limit: 100 });
    if (!resp.ok || !Array.isArray(resp.body)) return;
    state.sessions = resp.body.filter(row => {
      if (String(row?.status || '') !== 'running') return false;
      const backend = String(row?.backend_source || row?.source || '').toLowerCase();
      return spec.backendMatch(backend);
    });
    renderAgentSigninSection();
  } catch (_) {
    /* the next poll or manual refresh recovers */
  }
}

function agentSigninReloadSession(provider, sessionId) {
  const state = agentSigninState[provider];
  state.reloadRequested.add(sessionId);
  renderAgentSigninSection();
  const sent = dispatchSessionControlMsg(
    { action: 'reload_credentials', session_id: sessionId },
    {
      onError: err => {
        state.reloadRequested.delete(sessionId);
        showControlToast?.('error', `Reload failed: ${err?.message || err}`);
        renderAgentSigninSection();
      },
    }
  );
  if (sent === false) {
    state.reloadRequested.delete(sessionId);
    renderAgentSigninSection();
    return;
  }
  showControlToast?.('success', 'Reload requested — the session restarts on the new account');
}

function renderAgentSigninSection() {
  const mount = document.getElementById('agent-signin-section');
  if (!mount) return;
  // Never clobber a code paste mid-typing.
  if (
    mount.contains(document.activeElement) &&
    document.activeElement.matches('input, textarea, select')
  ) {
    return;
  }
  mount.innerHTML = '';
  for (const provider of Object.keys(AGENT_SIGNIN_PROVIDERS)) {
    mount.appendChild(agentSigninProviderCard(provider));
  }
  agentSigninEnsureTicker();
}

function agentSigninProviderCard(provider) {
  const spec = AGENT_SIGNIN_PROVIDERS[provider];
  const state = agentSigninState[provider];
  const card = document.createElement('div');
  card.className = `vault-card agent-signin agent-signin-${provider}`;

  const note = (text, cls = 'vault-note') => {
    const el = document.createElement('div');
    el.className = cls;
    el.textContent = text;
    card.appendChild(el);
    return el;
  };
  const actionsRow = (...children) => {
    const row = document.createElement('div');
    row.className = 'vault-actions';
    for (const child of children) if (child) row.appendChild(child);
    card.appendChild(row);
    return row;
  };
  const cancelButton = () =>
    vaultButton('Cancel', () => agentSigninCancel(provider), { danger: true });

  const head = document.createElement('div');
  head.className = 'vault-status-line agent-signin-head';
  const title = document.createElement('span');
  title.className = 'lbl';
  title.style.fontWeight = '600';
  title.textContent = spec.label;
  head.appendChild(title);
  card.appendChild(head);

  // Availability honesty first (same pattern as the fueling panel).
  const avail = daemonApi.availability(spec.statusMethod);
  if (avail.reason === 'denied') {
    note("This session's role can't run credential ceremonies — sign-in needs credentials.manage.");
    return card;
  }
  if (avail.reason === 'unsupported') {
    note(spec.unsupportedNote);
    return card;
  }

  const status = state.status;
  const phase = agentSigninPhase(provider);
  const chipDefs = {
    idle: ['ready', ''],
    starting: ['starting', ''],
    awaiting_browser: ['waiting for you', 'warn'],
    awaiting_code: ['waiting for the code', 'warn'],
    awaiting_user: ['waiting for you', 'warn'],
    verifying: ['verifying', 'warn'],
    success: ['signed in', 'ok'],
    failed: ['failed', 'warn'],
    cancelled: ['cancelled', ''],
    timed_out: ['timed out', 'warn'],
  };
  const [chipText, chipCls] = chipDefs[phase] || [phase, ''];
  const chip = document.createElement('span');
  chip.className = 'vault-chip';
  chip.textContent = chipText;
  if (chipCls) chip.classList.add(chipCls);
  head.appendChild(chip);
  const lineText = {
    ...spec.lines,
    failed: 'The sign-in did not complete.',
    cancelled: 'Sign-in cancelled — the previous account (if any) still works.',
    timed_out: 'The sign-in timed out — nothing changed on this machine.',
  }[phase];
  head.appendChild(document.createTextNode(lineText || ''));

  if (state.lastError) {
    note(state.lastError, 'vault-error');
  }
  if (status?.error && ['failed', 'timed_out'].includes(phase)) {
    note(status.error, 'vault-error');
  }

  if (phase === 'idle' || ['failed', 'cancelled', 'timed_out'].includes(phase)) {
    // The shared single-flight slot: the daemon reports which provider
    // holds it, and a start here would 409 until that ceremony ends.
    const busyWith = String(status?.busy_with || '');
    if (busyWith) {
      const other = AGENT_SIGNIN_PROVIDERS[busyWith]?.label || busyWith;
      note(
        `A ${other} sign-in ceremony is running — one credential ceremony at a time on this daemon.`
      );
      return card;
    }
    note(spec.blastCopy, 'vault-warning');
    actionsRow(
      vaultButton(
        phase === 'idle' ? spec.startLabel : 'Try again',
        () => agentSigninStart(provider),
        { primary: true }
      )
    );
    return card;
  }

  if (phase === 'starting' || phase === 'verifying') {
    actionsRow(cancelButton());
    return card;
  }

  if (['awaiting_browser', 'awaiting_code', 'awaiting_user'].includes(phase)) {
    const url = String(status?.url || '');
    const stepOne = document.createElement('div');
    stepOne.className = 'agent-signin-step';
    const stepOneLabel = document.createElement('div');
    stepOneLabel.className = 'vault-note';
    stepOneLabel.textContent = '1. Sign in with your browser';
    stepOne.appendChild(stepOneLabel);
    if (url) {
      const openRow = document.createElement('div');
      openRow.className = 'vault-actions';
      openRow.appendChild(
        vaultButton(spec.openLabel, () => {
          window.open(url, '_blank', 'noopener');
        }, { primary: true })
      );
      openRow.appendChild(
        vaultButton('Copy link', () => {
          navigator.clipboard
            ?.writeText(url)
            .then(() => showControlToast?.('success', 'Sign-in link copied'))
            .catch(() => showControlToast?.('error', 'Copy failed'));
        })
      );
      stepOne.appendChild(openRow);
      const checkNote = document.createElement('div');
      checkNote.className = 'vault-note';
      checkNote.textContent = spec.checkNote;
      stepOne.appendChild(checkNote);
      const urlLine = document.createElement('div');
      urlLine.className = 'agent-signin-url';
      urlLine.textContent = url;
      stepOne.appendChild(urlLine);
    } else {
      const waiting = document.createElement('div');
      waiting.className = 'vault-note';
      waiting.textContent = 'Waiting for the CLI to produce the sign-in link…';
      stepOne.appendChild(waiting);
    }
    card.appendChild(stepOne);

    if (phase === 'awaiting_user') {
      // Codex device step: the owner reads the one-time code off this
      // card and types it into OpenAI's page.
      const stepTwo = document.createElement('div');
      stepTwo.className = 'agent-signin-step';
      const stepTwoLabel = document.createElement('div');
      stepTwoLabel.className = 'vault-note';
      stepTwoLabel.textContent = '2. Enter this one-time code on the OpenAI page';
      stepTwo.appendChild(stepTwoLabel);
      const userCode = String(status?.user_code || '');
      if (userCode) {
        const codeRow = document.createElement('div');
        codeRow.className = 'vault-actions agent-signin-device-row';
        const codeEl = document.createElement('div');
        codeEl.className = 'agent-signin-device-code';
        codeEl.textContent = userCode;
        codeRow.appendChild(codeEl);
        codeRow.appendChild(
          vaultButton('Copy code', () => {
            navigator.clipboard
              ?.writeText(userCode)
              .then(() => showControlToast?.('success', 'One-time code copied'))
              .catch(() => showControlToast?.('error', 'Copy failed'));
          })
        );
        stepTwo.appendChild(codeRow);
        const deadlineMs = Number(status?.deadline_unix_ms || 0);
        if (deadlineMs) {
          const countdown = document.createElement('div');
          countdown.className = 'vault-note agent-signin-countdown';
          countdown.dataset.signinCountdown = String(deadlineMs);
          countdown.textContent = agentSigninCountdownText(deadlineMs);
          stepTwo.appendChild(countdown);
        }
      } else {
        const waiting = document.createElement('div');
        waiting.className = 'vault-note';
        waiting.textContent = 'Waiting for the CLI to produce the one-time code…';
        stepTwo.appendChild(waiting);
      }
      if (spec.deviceWarning) {
        const warn = document.createElement('div');
        warn.className = 'vault-warning';
        warn.textContent = spec.deviceWarning;
        stepTwo.appendChild(warn);
      }
      card.appendChild(stepTwo);
    } else {
      // Claude paste step: the code Anthropic shows comes back here.
      const stepTwo = document.createElement('div');
      stepTwo.className = 'agent-signin-step';
      const stepTwoLabel = document.createElement('div');
      stepTwoLabel.className = 'vault-note';
      stepTwoLabel.textContent = '2. Paste the code Anthropic shows you';
      stepTwo.appendChild(stepTwoLabel);
      const codeRow = document.createElement('div');
      codeRow.className = 'vault-actions';
      const codeInput = document.createElement('input');
      codeInput.type = 'text';
      codeInput.className = 'agent-signin-code';
      codeInput.placeholder = 'Paste code here';
      codeInput.autocomplete = 'off';
      codeInput.spellcheck = false;
      const submit = () => {
        const code = codeInput.value.trim();
        if (!code) return;
        agentSigninSubmitCode(provider, code);
      };
      codeInput.addEventListener('keydown', event => {
        if (event.key === 'Enter') {
          event.preventDefault();
          submit();
        }
      });
      codeRow.appendChild(codeInput);
      codeRow.appendChild(vaultButton('Submit code', submit, { primary: true }));
      stepTwo.appendChild(codeRow);
      card.appendChild(stepTwo);
    }

    actionsRow(cancelButton());
    return card;
  }

  if (phase === 'success') {
    const account = status?.account || null;
    if (account) {
      const row = document.createElement('div');
      row.className = 'vault-entry-row';
      const lbl = document.createElement('span');
      lbl.className = 'lbl';
      lbl.textContent = account.email || 'Signed in';
      row.appendChild(lbl);
      if (account.subscription_type) {
        const plan = document.createElement('span');
        plan.className = 'vault-chip ok';
        plan.textContent = account.subscription_type;
        row.appendChild(plan);
      }
      if (account.org_name) {
        const org = document.createElement('span');
        org.className = 'vault-chip';
        org.textContent = account.org_name;
        row.appendChild(org);
      }
      if (account.auth_method && !account.subscription_type) {
        const method = document.createElement('span');
        method.className = 'vault-chip ok';
        method.textContent = account.auth_method;
        row.appendChild(method);
      }
      card.appendChild(row);
    } else {
      note('Signed in (the CLI did not report account details).');
    }

    // The value half: running sessions keep the OLD account until their
    // backend process restarts — offer the per-session reload (graceful
    // in-place respawn, resume-attached).
    agentSigninRefreshSessions(provider).catch(() => {});
    const reloadHead = document.createElement('div');
    reloadHead.className = 'vault-status-line';
    const reloadTitle = document.createElement('span');
    reloadTitle.className = 'lbl';
    reloadTitle.style.fontWeight = '600';
    reloadTitle.textContent = spec.sessionsTitle;
    reloadHead.appendChild(reloadTitle);
    card.appendChild(reloadHead);
    if (!state.sessions.length) {
      note(spec.noSessionsNote);
    } else {
      note('Running sessions keep the old account until reloaded. Reloading restarts the backend on the same conversation; a mid-turn session is interrupted first.');
      const list = document.createElement('div');
      list.className = 'vault-entry-list';
      for (const session of state.sessions) {
        const sessionId = String(session.session_id || '');
        const row = document.createElement('div');
        row.className = 'vault-entry-row';
        const lbl = document.createElement('span');
        lbl.className = 'lbl';
        lbl.textContent = session.name || sessionId.slice(0, 12) || 'session';
        const kindChip = document.createElement('span');
        kindChip.className = 'vault-chip';
        kindChip.textContent = spec.sessionKind;
        const actions = document.createElement('span');
        actions.className = 'vault-entry-actions';
        if (state.reloadRequested.has(sessionId)) {
          const done = document.createElement('span');
          done.className = 'vault-chip ok';
          done.textContent = 'reload requested';
          actions.appendChild(done);
        } else {
          actions.appendChild(
            vaultButton('Reload credentials', () => agentSigninReloadSession(provider, sessionId))
          );
        }
        row.append(lbl, kindChip, actions);
        list.appendChild(row);
      }
      card.appendChild(list);
    }
    actionsRow(vaultButton('Sign in again', () => agentSigninStart(provider)));
    return card;
  }

  return card;
}

async function vaultFuelEntry(entry) {
  const kind = vaultEntryLeaseKind(entry);
  if (!kind) return;
  vaultLeaseState.lastError = '';
  vaultLeaseState.busy = true;
  renderAccessVaultSection();
  try {
    const oauth = kind.startsWith('oauth:');
    const fullCredential = oauth && vaultOauthLeasesEnabled();
    const material =
      oauth && !fullCredential
        ? await vaultOauthAccessTokenMaterial(entry, kind)
        : String(entry.secret);
    const result = await vaultLeaseRpc('api_credential_lease_grant', {
      kind,
      label: entry.label || vaultProviderLabel(entry.provider),
      material,
      ...(oauth ? { mode: fullCredential ? 'full_credential' : 'access_token' } : {}),
      ttl_ms: VAULT_LEASE_TTL_MS,
      offline_ms: vaultOfflineMs(),
    });
    if (result?.lease_id) {
      vaultOwnLeaseIds.add(result.lease_id);
      vaultEnsureRenewLoop();
    }
    await vaultRefreshLeases({ force: true });
  } catch (err) {
    vaultLeaseState.lastError = `Fueling failed: ${err?.message || err}`;
  } finally {
    vaultLeaseState.busy = false;
    renderAccessVaultSection();
  }
}

async function vaultRevokeLease(leaseId) {
  vaultLeaseState.lastError = '';
  try {
    await vaultLeaseRpc('api_credential_lease_revoke', { lease_id: leaseId });
    vaultOwnLeaseIds.delete(leaseId);
    await vaultRefreshLeases({ force: true });
  } catch (err) {
    vaultLeaseState.lastError = `Revocation failed: ${err?.message || err}`;
    renderAccessVaultSection();
  }
}

/* Connected renewal: every granting tab renews its own leases while the
   tunnel is up; a failed renewal (revoked elsewhere, expiry, detach)
   just drops the lease from this tab's renew set. Access-token oauth
   leases whose provider token nears expiry re-grant freshly refreshed
   material instead — a plain renew would keep a lease alive around a
   token that is about to die. */
async function vaultRenewOwnLeasesOnce() {
  if (!vaultLeaseTransportReady()) return;
  for (const leaseId of Array.from(vaultOwnLeaseIds)) {
    const lease = vaultLeaseState.leases.find(l => l.lease_id === leaseId);
    if (lease?.mode === 'access_token' && vaultState.status === 'unlocked') {
      const entry = vaultState.entries.find(e => vaultEntryLeaseKind(e) === lease.kind);
      if (entry) {
        let stale = true;
        try {
          stale =
            vaultOauthExpiryMs(lease.kind, JSON.parse(String(entry.secret))) - Date.now() <
            VAULT_OAUTH_REFRESH_MARGIN_MS;
        } catch (_) {}
        if (stale) {
          // Replaces the lease on the daemon; the ids reconcile on the
          // forced status refresh below. On failure the stale lease is
          // deliberately left un-renewed: its token is dying anyway,
          // and keeping it alive would just read as phantom fuel.
          await vaultFuelEntry(entry);
          continue;
        }
      }
    }
    try {
      await vaultLeaseRpc('api_credential_lease_renew', { lease_id: leaseId });
    } catch (err) {
      console.warn(`[vault] lease renewal failed for ${leaseId}:`, err?.message || err);
      vaultOwnLeaseIds.delete(leaseId);
    }
  }
  vaultRefreshLeases({ force: true }).catch(() => {});
}

function vaultEnsureRenewLoop() {
  if (vaultRenewTimer || !vaultOwnLeaseIds.size) return;
  vaultRenewTimer = window.setInterval(() => {
    if (!vaultOwnLeaseIds.size) {
      window.clearInterval(vaultRenewTimer);
      vaultRenewTimer = null;
      return;
    }
    vaultRenewOwnLeasesOnce().catch(() => {});
  }, VAULT_LEASE_RENEW_EVERY_MS);
}

function vaultLeaseExpiryText(lease) {
  const remaining = Number(lease.expires_at_unix_ms || 0) - Date.now();
  if (remaining <= 0) return 'expiring';
  if (remaining < 90 * 1000) return 'expires in under 2 min';
  if (remaining < 60 * 60 * 1000) return `expires in ${Math.round(remaining / 60000)} min`;
  if (remaining < 48 * 60 * 60 * 1000) return `expires in ${Math.round(remaining / 3600000)} h`;
  return `expires in ${Math.round(remaining / 86400000)} d`;
}

/* ── Client egress: relay provider calls through this browser ──
   The zero-lease fueling mode (credential custody, rollout step 5): the
   daemon ships each provider request here auth-less over the verified
   tunnel; this tab attaches the key from the unlocked vault, performs
   the fetch against the provider's fixed origin, and streams the body
   back under the daemon's credit window. The credential never leaves
   the browser, and the capability dies the moment this tab detaches.
   Anthropic and Gemini only — OpenAI refuses browser CORS. */

const VAULT_EGRESS_HOSTS = {
  'api_key:anthropic': 'api.anthropic.com',
  'api_key:gemini': 'generativelanguage.googleapis.com',
};
const VAULT_EGRESS_CHUNK_BYTES = 16 * 1024;
const vaultEgressJobs = new Map();
const vaultEgressState = {
  enabled: new Set(),    // kinds enabled for this daemon (persisted)
  registered: new Set(), // kinds the daemon acknowledged this session
  allowHosts: {},        // validator-only host overrides (debug handle)
  lastError: '',
};

function vaultEgressLoadEnabled() {
  try {
    const stored = JSON.parse(localStorage.getItem(vaultLeaseDaemonKey('egress_kinds')) || '[]');
    vaultEgressState.enabled = new Set(Array.isArray(stored) ? stored.map(String) : []);
  } catch {
    vaultEgressState.enabled = new Set();
  }
}
vaultEgressLoadEnabled();

function vaultEgressPersistEnabled() {
  localStorage.setItem(
    vaultLeaseDaemonKey('egress_kinds'),
    JSON.stringify(Array.from(vaultEgressState.enabled))
  );
}

/* The vault entry that fuels a relay kind; API-key entries only. */
function vaultEgressEntryFor(kind) {
  if (vaultState.status !== 'unlocked') return null;
  const provider = String(kind || '').startsWith('api_key:') ? String(kind).slice(8) : '';
  if (!provider) return null;
  const usable = vaultState.entries.filter(vaultEntryUsableHere);
  return (
    usable.find(e => e.kind === 'api_key' && e.provider === provider && e.secret && !e.voice) ||
    usable.find(e => e.kind === 'api_key' && e.provider === provider && e.secret) ||
    null
  );
}

function vaultEgressDesiredKinds() {
  return Array.from(vaultEgressState.enabled).filter(kind => vaultEgressEntryFor(kind));
}

/* Reconcile the daemon-side registry with what this tab wants to relay.
   Registration is idempotent and re-run on every lease refresh, so a
   reconnected session (new session id) heals itself within one cycle. */
let vaultEgressEnsureInFlight = false;
async function vaultEgressEnsure() {
  if (!vaultLeaseTransportReady() || vaultEgressEnsureInFlight) return;
  vaultEgressEnsureInFlight = true;
  try {
    const desired = vaultEgressDesiredKinds();
    const stale = Array.from(vaultEgressState.registered).filter(kind => !desired.includes(kind));
    if (stale.length) {
      await vaultLeaseRpc('api_credential_egress_unregister', { kinds: stale }).catch(() => {});
      for (const kind of stale) vaultEgressState.registered.delete(kind);
    }
    if (desired.length) {
      const result = await vaultLeaseRpc('api_credential_egress_register', { kinds: desired });
      vaultEgressState.registered = new Set(result?.registered || desired);
      vaultEgressState.lastError = '';
    }
  } catch (err) {
    vaultEgressState.lastError = String(err?.message || err);
  } finally {
    vaultEgressEnsureInFlight = false;
  }
}

function vaultEgressSendFrame(frame) {
  try {
    dashboardControlTransport?.sendFrame?.(frame);
  } catch (err) {
    console.warn('[vault-egress] frame send failed:', err?.message || err);
  }
}

function vaultEgressB64Decode(data) {
  const bin = atob(String(data || ''));
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

function vaultEgressB64Encode(bytes) {
  let bin = '';
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
  return btoa(bin);
}

function vaultEgressFail(id, error) {
  if (vaultEgressJobs.delete(id)) {
    vaultEgressSendFrame({ t: 'egress_error', id, error: String(error).slice(0, 400) });
  }
}

function vaultEgressHandleFrame(msg) {
  const id = String(msg.id || '');
  if (!id) return;
  if (msg.t === 'egress_request') {
    vaultEgressJobs.set(id, {
      kind: String(msg.kind || ''),
      method: String(msg.method || 'POST'),
      url: String(msg.url || ''),
      headers: Array.isArray(msg.headers) ? msg.headers : [],
      credit: Number(msg.credit) || 1024 * 1024,
      chunks: [],
      controller: null,
      creditWaiter: null,
      canceled: false,
    });
    return;
  }
  const job = vaultEgressJobs.get(id);
  if (!job) return;
  if (msg.t === 'egress_request_chunk') {
    try {
      job.chunks.push(vaultEgressB64Decode(msg.data));
    } catch {
      vaultEgressFail(id, 'malformed request chunk');
    }
    return;
  }
  if (msg.t === 'egress_request_end') {
    vaultEgressExecute(id).catch(err => vaultEgressFail(id, String(err?.message || err)));
    return;
  }
  if (msg.t === 'egress_ack') {
    job.credit += Number(msg.bytes) || 0;
    if (job.creditWaiter) {
      const wake = job.creditWaiter;
      job.creditWaiter = null;
      wake();
    }
    return;
  }
  if (msg.t === 'egress_cancel') {
    job.canceled = true;
    try {
      job.controller?.abort();
    } catch (_) {}
    if (job.creditWaiter) {
      const wake = job.creditWaiter;
      job.creditWaiter = null;
      wake();
    }
    vaultEgressJobs.delete(id);
  }
}

async function vaultEgressExecute(id) {
  const job = vaultEgressJobs.get(id);
  if (!job) return;
  if (!vaultEgressState.enabled.has(job.kind)) {
    return vaultEgressFail(id, `relaying ${job.kind} is not enabled in this tab`);
  }
  const entry = vaultEgressEntryFor(job.kind);
  if (!entry) {
    return vaultEgressFail(id, 'the vault is locked or holds no key for this kind — unlock it and retry');
  }
  let host = '';
  try {
    host = new URL(job.url).host;
  } catch {
    return vaultEgressFail(id, 'malformed egress URL');
  }
  // The relay only ever talks to the provider's own origin — a
  // compromised daemon cannot turn this tab into an open proxy.
  const allowedHost = vaultEgressState.allowHosts[job.kind] || VAULT_EGRESS_HOSTS[job.kind];
  if (!allowedHost || host !== allowedHost) {
    return vaultEgressFail(id, `egress to ${host} is not allowed for ${job.kind} (expected ${allowedHost})`);
  }

  const headers = {};
  for (const pair of job.headers) {
    if (Array.isArray(pair) && pair.length === 2) headers[String(pair[0])] = String(pair[1]);
  }
  if (job.kind === 'api_key:anthropic') {
    headers['x-api-key'] = String(entry.secret);
    headers['anthropic-dangerous-direct-browser-access'] = 'true';
  } else if (job.kind === 'api_key:gemini') {
    headers['x-goog-api-key'] = String(entry.secret);
  }

  let total = 0;
  for (const chunk of job.chunks) total += chunk.length;
  const body = new Uint8Array(total);
  let offset = 0;
  for (const chunk of job.chunks) {
    body.set(chunk, offset);
    offset += chunk.length;
  }
  job.chunks = [];
  job.controller = new AbortController();

  let resp;
  try {
    resp = await fetch(job.url, {
      method: job.method,
      headers,
      body: total ? body : undefined,
      signal: job.controller.signal,
    });
  } catch (err) {
    return vaultEgressFail(id, `fetch failed: ${err?.message || err}`);
  }
  vaultEgressSendFrame({ t: 'egress_response', id, status: resp.status });
  try {
    const reader = resp.body?.getReader();
    if (reader) {
      for (;;) {
        const { done, value } = await reader.read();
        if (done || job.canceled) break;
        for (let i = 0; i < value.length && !job.canceled; i += VAULT_EGRESS_CHUNK_BYTES) {
          const slice = value.subarray(i, Math.min(i + VAULT_EGRESS_CHUNK_BYTES, value.length));
          while (job.credit < slice.length && !job.canceled) {
            await new Promise(resolve => {
              job.creditWaiter = resolve;
            });
          }
          if (job.canceled) break;
          job.credit -= slice.length;
          vaultEgressSendFrame({ t: 'egress_chunk', id, data: vaultEgressB64Encode(slice) });
        }
      }
    }
    if (!job.canceled) vaultEgressSendFrame({ t: 'egress_end', id });
    vaultEgressJobs.delete(id);
  } catch (err) {
    if (job.canceled) {
      vaultEgressJobs.delete(id);
    } else {
      vaultEgressFail(id, `response stream failed: ${err?.message || err}`);
    }
  }
}

/* ── Vault UI (v1: Access → Advanced · ui-v2: the #vault destination) ── */

function vaultProviderLabel(provider) {
  if (provider === 'anthropic') return 'Anthropic';
  if (provider === 'openai') return 'OpenAI';
  if (provider === 'gemini') return 'Gemini';
  if (provider === 'codex') return 'Codex (subscription)';
  if (provider === 'claude-code') return 'Claude Code (subscription)';
  return provider || 'custom';
}

function vaultButton(label, onClick, { primary = false, danger = false } = {}) {
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = `ui-btn${primary ? ' primary' : ''}${danger ? ' danger' : ''}`;
  btn.textContent = label;
  btn.addEventListener('click', onClick);
  return btn;
}

function vaultRenderCeremony(card) {
  const note = document.createElement('div');
  note.className = 'vault-warning';
  note.textContent =
    'Write these 12 words down and keep them offline. The phrase is the only unlocker that survives losing every passkey, and it is shown exactly once.';
  card.appendChild(note);

  const grid = document.createElement('div');
  grid.className = 'vault-words';
  vaultCeremony.phrase.split(' ').forEach((word, i) => {
    const w = document.createElement('span');
    w.className = 'w';
    const n = document.createElement('span');
    n.className = 'n';
    n.textContent = String(i + 1);
    w.append(n, document.createTextNode(word));
    grid.appendChild(w);
  });
  card.appendChild(grid);

  const actions = document.createElement('div');
  actions.className = 'vault-actions';
  actions.appendChild(
    vaultButton('I saved the phrase — create the vault', async () => {
      try {
        await vaultCreate(vaultCeremony.phrase);
        vaultCeremony = null;
      } catch (err) {
        vaultState.lastError = `Vault creation failed: ${err?.message || err}`;
      }
      renderAccessVaultSection();
    }, { primary: true })
  );
  actions.appendChild(
    vaultButton('Copy phrase', () => {
      navigator.clipboard?.writeText(vaultCeremony.phrase).catch(() => {});
    })
  );
  actions.appendChild(
    vaultButton('Cancel', () => {
      vaultCeremony = null;
      renderAccessVaultSection();
    })
  );
  card.appendChild(actions);
}

function vaultRenderLocked(card) {
  const actions = document.createElement('div');
  actions.className = 'vault-actions';
  actions.appendChild(vaultButton('Unlock with passkey', () => vaultUnlockWithPasskey(), { primary: true }));
  card.appendChild(actions);

  const phraseRow = document.createElement('div');
  phraseRow.className = 'vault-actions';
  const phraseInput = document.createElement('input');
  phraseInput.type = 'password';
  phraseInput.className = 'vault-phrase-input';
  phraseInput.placeholder = 'or type the 12-word recovery phrase';
  phraseInput.autocomplete = 'off';
  phraseInput.style.flex = '1';
  const phraseBtn = vaultButton('Unlock with phrase', async () => {
    await vaultUnlockWithPhrase(phraseInput.value);
  });
  phraseInput.addEventListener('keydown', e => {
    if (e.key === 'Enter') phraseBtn.click();
  });
  phraseRow.append(phraseInput, phraseBtn);
  card.appendChild(phraseRow);
}

function vaultRenderUnlockers(card) {
  const list = document.createElement('div');
  list.className = 'vault-unlockers';
  for (const envelope of vaultState.blob?.envelopes || []) {
    const row = document.createElement('div');
    row.className = 'u';
    const kind = envelope.kind === 'phrase' ? 'Recovery phrase' : 'Passkey';
    const current = envelope.id && envelope.id === vaultState.matchedEnvelopeId ? ' — unlocked this session' : '';
    row.textContent = `${kind}: ${envelope.label || envelope.id || ''}${current}`;
    list.appendChild(row);
  }
  const fold = document.createElement('details');
  fold.className = 'acc-fold';
  const summary = document.createElement('summary');
  summary.textContent = `Unlockers (${(vaultState.blob?.envelopes || []).length})`;
  const body = document.createElement('div');
  body.className = 'acc-fold-body';
  body.appendChild(list);
  fold.append(summary, body);
  card.appendChild(fold);
}

function vaultRenderEntries(card) {
  const list = document.createElement('div');
  list.className = 'vault-entry-list';
  if (!vaultState.entries.length) {
    const empty = document.createElement('div');
    empty.className = 'vault-note';
    empty.textContent = 'No credentials stored yet. Anything added here syncs end-to-end encrypted across your devices and never reaches a server in the clear.';
    list.appendChild(empty);
  }
  for (const entry of vaultState.entries) {
    const row = document.createElement('div');
    row.className = 'vault-entry-row';
    const lbl = document.createElement('span');
    lbl.className = 'lbl';
    lbl.textContent = entry.label || vaultProviderLabel(entry.provider);
    const chip = document.createElement('span');
    chip.className = 'vault-chip';
    chip.textContent = vaultProviderLabel(entry.provider);
    const kindChip = document.createElement('span');
    kindChip.className = 'vault-chip';
    kindChip.textContent = entry.kind === 'oauth' ? 'OAuth' : 'API key';
    const trustedOnly = vaultEntryUnsealPolicy(entry) === 'trusted';
    const usableHere = vaultEntryUsableHere(entry);
    let policyChip = null;
    if (trustedOnly) {
      policyChip = document.createElement('span');
      policyChip.className = usableHere ? 'vault-chip' : 'vault-chip warn';
      policyChip.textContent = usableHere ? 'trusted origins' : 'sealed here';
      policyChip.title = usableHere
        ? 'This credential only works from trusted origins (direct or app) — hosted tabs cannot reveal, fuel, or relay it.'
        : 'This credential is marked trusted-origins-only and this dashboard arrived through a hosted tab: it stays sealed here — no reveal, fueling, or relay. It still syncs with the vault.';
    }
    const secret = document.createElement('span');
    secret.className = 'secret';
    const secretValue = String(entry.secret || '');
    secret.textContent = usableHere && vaultRevealedEntries.has(entry.id)
      ? secretValue || '(token set)'
      : secretValue
        ? `••••${secretValue.slice(-4)}`
        : '(token set)';
    const actions = document.createElement('span');
    actions.className = 'vault-entry-actions';
    if (secretValue) {
      const reveal = vaultButton(usableHere && vaultRevealedEntries.has(entry.id) ? 'Hide' : 'Reveal', () => {
        if (vaultRevealedEntries.has(entry.id)) vaultRevealedEntries.delete(entry.id);
        else vaultRevealedEntries.add(entry.id);
        renderAccessVaultSection();
      });
      const copy = vaultButton('Copy', () => {
        navigator.clipboard?.writeText(secretValue).catch(() => {});
      });
      if (!usableHere) {
        reveal.disabled = true;
        copy.disabled = true;
        reveal.title = copy.title = 'Sealed in hosted tabs — this entry is marked trusted-origins-only.';
      }
      actions.append(reveal, copy);
    }
    const policyBtn = vaultButton(trustedOnly ? 'Allow anywhere' : 'Trusted only', () => {
      vaultUpsertEntry({ id: entry.id, unseal_policy: trustedOnly ? 'any' : 'trusted' });
      renderAccessVaultSection();
    });
    policyBtn.title = trustedOnly
      ? 'Let every dashboard this vault opens in use this credential again.'
      : 'Seal this credential against hosted tabs: only direct or app origins may reveal, fuel, or relay it. (Client-side policy — it guards against mistakes, not a malicious page.)';
    actions.appendChild(policyBtn);
    actions.appendChild(
      vaultButton('Remove', () => {
        vaultRemoveEntry(entry.id);
        renderAccessVaultSection();
      }, { danger: true })
    );
    if (policyChip) row.append(lbl, chip, kindChip, policyChip, secret, actions);
    else row.append(lbl, chip, kindChip, secret, actions);
    list.appendChild(row);
  }
  card.appendChild(list);
}

function vaultRenderAddForm(card) {
  const fold = document.createElement('details');
  fold.className = 'acc-fold';
  const summary = document.createElement('summary');
  summary.textContent = 'Add a credential';
  const body = document.createElement('div');
  body.className = 'acc-fold-body';
  const grid = document.createElement('div');
  grid.className = 'vault-form-grid';

  const kindLabel = document.createElement('label');
  kindLabel.textContent = 'Kind';
  const kindSelect = document.createElement('select');
  for (const [value, label] of [['api_key', 'API key'], ['oauth', 'Subscription OAuth (auth-file JSON)']]) {
    const option = document.createElement('option');
    option.value = value;
    option.textContent = label;
    kindSelect.appendChild(option);
  }
  const providerLabel = document.createElement('label');
  providerLabel.textContent = 'Provider';
  const providerSelect = document.createElement('select');
  const fillProviders = () => {
    providerSelect.innerHTML = '';
    const providers = kindSelect.value === 'oauth'
      ? ['codex', 'claude-code']
      : ['anthropic', 'openai', 'gemini'];
    for (const provider of providers) {
      const option = document.createElement('option');
      option.value = provider;
      option.textContent = vaultProviderLabel(provider);
      providerSelect.appendChild(option);
    }
  };
  fillProviders();
  const labelLabel = document.createElement('label');
  labelLabel.textContent = 'Label';
  const labelInput = document.createElement('input');
  labelInput.type = 'text';
  labelInput.placeholder = 'e.g. Personal Anthropic';
  labelInput.autocomplete = 'off';
  const secretLabel = document.createElement('label');
  secretLabel.textContent = 'API key';
  const secretInput = document.createElement('input');
  secretInput.type = 'password';
  secretInput.placeholder = 'sk-…';
  secretInput.autocomplete = 'off';
  const secretArea = document.createElement('textarea');
  secretArea.className = 'vault-phrase-input';
  secretArea.rows = 4;
  secretArea.placeholder = 'Paste the agent auth file JSON (Codex: ~/.codex/auth.json · Claude Code: ~/.claude/.credentials.json)';
  secretArea.style.display = 'none';
  kindSelect.addEventListener('change', () => {
    fillProviders();
    const oauth = kindSelect.value === 'oauth';
    secretLabel.textContent = oauth ? 'Auth JSON' : 'API key';
    secretInput.style.display = oauth ? 'none' : '';
    secretArea.style.display = oauth ? '' : 'none';
  });
  const policyLabel = document.createElement('label');
  policyLabel.textContent = 'Unseal where?';
  const policySelect = document.createElement('select');
  for (const [value, label, title] of [
    ['any', 'Anywhere this vault opens', 'The default: usable from every dashboard that can open your vault.'],
    ['trusted', 'Trusted origins only (direct / app)', 'Usable from this daemon-origin/native vault. Any future hosted vault client must keep it sealed: no reveal, fueling, or relay. Client-side policy guards against mistakes, not malicious served code.'],
  ]) {
    const option = document.createElement('option');
    option.value = value;
    option.textContent = label;
    option.title = title;
    policySelect.appendChild(option);
  }
  grid.append(kindLabel, kindSelect, providerLabel, providerSelect, labelLabel, labelInput, policyLabel, policySelect, secretLabel, secretInput);
  body.appendChild(grid);
  body.appendChild(secretArea);

  const actions = document.createElement('div');
  actions.className = 'vault-actions';
  actions.style.marginTop = '8px';
  actions.appendChild(
    vaultButton('Add to vault', () => {
      const oauth = kindSelect.value === 'oauth';
      const secretValue = (oauth ? secretArea.value : secretInput.value).trim();
      if (!secretValue) return;
      if (oauth) {
        try {
          JSON.parse(secretValue);
        } catch {
          vaultState.lastError = 'That does not parse as JSON — paste the whole auth file.';
          renderAccessVaultSection();
          return;
        }
      }
      vaultUpsertEntry({
        kind: kindSelect.value,
        provider: providerSelect.value,
        label: labelInput.value.trim() ||
          (oauth ? vaultProviderLabel(providerSelect.value) : `${vaultProviderLabel(providerSelect.value)} key`),
        secret: secretValue,
        ...(policySelect.value === 'trusted' ? { unseal_policy: 'trusted' } : {}),
      });
      secretInput.value = '';
      secretArea.value = '';
      labelInput.value = '';
      fold.open = false;
      renderAccessVaultSection();
    }, { primary: true })
  );
  body.appendChild(actions);
  fold.append(summary, body);
  card.appendChild(fold);
}

/* Fueling panel: this daemon's active leases + a fuel button per
   fuelable vault entry. The shipped path is the trusted loopback/direct-mTLS
   dashboard backed by this daemon's local vault. */
function vaultRenderFueling(card) {
  if (!vaultAvailable()) return;
  const head = document.createElement('div');
  head.className = 'vault-status-line';
  const title = document.createElement('span');
  title.className = 'lbl';
  title.style.fontWeight = '600';
  title.textContent = 'Fueling — this daemon';
  head.appendChild(title);
  card.appendChild(head);

  const fuelingNote = text => {
    const el = document.createElement('div');
    el.className = 'vault-note';
    el.textContent = text;
    card.appendChild(el);
  };
  // Availability honesty (transport F6): the daemon's features/status
  // answer BEFORE any RPC fires, so the panel names the actual cause —
  // a session whose role is refused the gate, a daemon predating the
  // family, or simply no tunnel yet — instead of the old conflated
  // "cannot manage leases" catch-all. The stored verdict covers the
  // race where a still-optimistic pre-flight let an RPC bounce.
  const avail = vaultLeaseAvailability();
  if (avail.reason === 'denied' || vaultLeaseState.availability === 'denied') {
    fuelingNote("This session's role can't manage credential leases — fueling needs credentials.manage.");
    return;
  }
  if (avail.reason === 'unsupported' || vaultLeaseState.availability === 'unsupported') {
    fuelingNote('This daemon predates credential leases — upgrade it to fuel from the vault.');
    return;
  }
  if (!avail.ok) {
    fuelingNote('Connect to the daemon to fuel it — leases travel only over the verified tunnel.');
    return;
  }
  vaultRefreshLeases().catch(() => {});

  if (vaultLeaseState.expiredNote) {
    const warning = document.createElement('div');
    warning.className = 'vault-warning';
    warning.textContent = vaultLeaseState.expiredNote;
    card.appendChild(warning);
  }
  // Transient lane failures (timeouts, channel drops) are errors, not
  // support verdicts — show them even before the first successful probe.
  if (vaultLeaseState.lastError && vaultLeaseState.supported !== false) {
    const error = document.createElement('div');
    error.className = 'vault-error';
    error.textContent = vaultLeaseState.lastError;
    card.appendChild(error);
  }

  const list = document.createElement('div');
  list.className = 'vault-entry-list';
  if (!vaultLeaseState.leases.length) {
    const empty = document.createElement('div');
    empty.className = 'vault-note';
    empty.textContent = 'No active leases. This daemon is running on its own local keys, or is unfueled.';
    list.appendChild(empty);
  }
  for (const lease of vaultLeaseState.leases) {
    const row = document.createElement('div');
    row.className = 'vault-entry-row';
    const lbl = document.createElement('span');
    lbl.className = 'lbl';
    lbl.textContent = lease.label || lease.kind;
    const kindChip = document.createElement('span');
    kindChip.className = 'vault-chip ok';
    kindChip.textContent = lease.kind;
    let modeChip = null;
    if (lease.mode === 'access_token' || lease.mode === 'full_credential') {
      modeChip = document.createElement('span');
      modeChip.className = lease.mode === 'access_token' ? 'vault-chip' : 'vault-chip warn';
      modeChip.textContent = lease.mode === 'access_token' ? 'access token' : 'full credential';
      modeChip.title =
        lease.mode === 'access_token'
          ? 'Short-lived access token; the refresh token never left the browser vault.'
          : 'Full auth file (refresh token included) — durable authority for the lease window.';
    }
    const detail = document.createElement('span');
    detail.className = 'secret';
    const renewing = vaultOwnLeaseIds.has(lease.lease_id) ? ', renewing from this tab' : '';
    detail.textContent = `${vaultLeaseExpiryText(lease)} · granted by ${lease.granted_by || 'unknown'} · used ${lease.use_count}×${renewing}`;
    const actions = document.createElement('span');
    actions.className = 'vault-entry-actions';
    actions.appendChild(
      vaultButton('Revoke', () => vaultRevokeLease(lease.lease_id), { danger: true })
    );
    if (modeChip) row.append(lbl, kindChip, modeChip, detail, actions);
    else row.append(lbl, kindChip, detail, actions);
    list.appendChild(row);
  }
  card.appendChild(list);

  // The offline knob IS the autonomy/security dial — surfaced at fueling
  // time, applying to grants made after the change.
  const knobRow = document.createElement('div');
  knobRow.className = 'vault-actions';
  const knobLabel = document.createElement('span');
  knobLabel.className = 'vault-note';
  knobLabel.textContent = 'After the last fueling session detaches, keep working:';
  const knob = document.createElement('select');
  for (const [value, label] of VAULT_OFFLINE_CHOICES) {
    const option = document.createElement('option');
    option.value = String(value);
    option.textContent = label;
    knob.appendChild(option);
  }
  knob.value = String(vaultOfflineMs());
  knob.addEventListener('change', () => {
    vaultSetOfflineMs(Number(knob.value));
  });
  knobRow.append(knobLabel, knob);
  card.appendChild(knobRow);

  const fuelable = vaultState.entries
    .map(entry => ({ entry, kind: vaultEntryLeaseKind(entry) }))
    .filter(item => item.kind);
  // No silent caps: say when entries exist but are sealed against this
  // context, instead of quietly rendering fewer fuel buttons.
  const sealed = vaultState.entries.filter(e => e.secret && !vaultEntryUsableHere(e)).length;
  if (sealed > 0) {
    const note = document.createElement('div');
    note.className = 'vault-note';
    note.textContent = `${sealed === 1 ? '1 credential is' : `${sealed} credentials are`} marked trusted-origins-only and stay${sealed === 1 ? 's' : ''} sealed in hosted tabs — no fueling from here.`;
    card.appendChild(note);
  }
  const fuelRow = document.createElement('div');
  fuelRow.className = 'vault-actions';
  for (const { entry, kind } of fuelable) {
    const active = vaultLeaseState.leases.some(lease => lease.kind === kind);
    const button = vaultButton(
      `${active ? 'Re-fuel' : 'Fuel'}: ${entry.label || vaultProviderLabel(entry.provider)}`,
      () => vaultFuelEntry(entry),
      { primary: !active }
    );
    button.disabled = vaultLeaseState.busy;
    if (kind.startsWith('oauth:')) {
      button.title = vaultOauthLeasesEnabled()
        ? 'Full-credential lease: the daemon holds the auth file (refresh token included) for the lease window.'
        : 'Access-token lease: this browser refreshes the token and leases only the short-lived result; the refresh token never leaves the vault.';
    }
    fuelRow.appendChild(button);
  }
  if (!fuelable.length) {
    const hint = document.createElement('span');
    hint.className = 'vault-note';
    hint.textContent = 'Add a provider credential above to fuel this daemon from the vault.';
    fuelRow.appendChild(hint);
  }
  card.appendChild(fuelRow);

  if (fuelable.some(item => item.kind.startsWith('oauth:'))) {
    const oauthRow = document.createElement('div');
    oauthRow.className = 'vault-actions';
    const toggle = document.createElement('input');
    toggle.type = 'checkbox';
    toggle.id = 'vault-oauth-lease-toggle';
    toggle.checked = vaultOauthLeasesEnabled();
    toggle.addEventListener('change', () => {
      vaultSetOauthLeasesEnabled(toggle.checked);
      renderAccessVaultSection();
    });
    const label = document.createElement('label');
    label.htmlFor = 'vault-oauth-lease-toggle';
    label.className = 'vault-note';
    label.textContent =
      'Fuel OAuth with the full credential file instead of browser-refreshed access tokens. While such a lease is live the daemon holds durable subscription authority; revocation then depends on lease discipline (worst case: the provider’s session-revocation page). Needed for Claude Code today — Anthropic’s token endpoint refuses browser refresh — and for autonomy beyond the provider’s access-token lifetime.';
    oauthRow.append(toggle, label);
    card.appendChild(oauthRow);
  }

  vaultRenderEgress(card);

  const renewNote = document.createElement('div');
  renewNote.className = 'vault-note';
  renewNote.textContent =
    'Leases renew every 5 minutes from this tab while it stays connected (access-token OAuth leases re-grant a freshly refreshed token as needed); material lives only in the daemon’s memory and dies on expiry, revocation, or restart.';
  card.appendChild(renewNote);
}

/* Client-egress section of the fueling panel: active relays (the path
   indicator) plus per-provider toggles for relaying through this tab. */
function vaultRenderEgress(card) {
  const relayable = ['api_key:anthropic', 'api_key:gemini'].filter(kind => vaultEgressEntryFor(kind));
  const relays = vaultLeaseState.egress || [];
  if (!relayable.length && !relays.length) return;

  const head = document.createElement('div');
  head.className = 'vault-status-line';
  const title = document.createElement('span');
  title.className = 'lbl';
  title.style.fontWeight = '600';
  title.textContent = 'Client egress — calls relayed through a browser';
  head.appendChild(title);
  card.appendChild(head);

  let mySessionId = '';
  try {
    mySessionId = String(window.intendantDashboardControl?.status?.()?.sessionId || '');
  } catch (_) {}

  if (relays.length) {
    const list = document.createElement('div');
    list.className = 'vault-entry-list';
    for (const relay of relays) {
      const row = document.createElement('div');
      row.className = 'vault-entry-row';
      const lbl = document.createElement('span');
      lbl.className = 'lbl';
      lbl.textContent = relay.kind;
      const chip = document.createElement('span');
      chip.className = 'vault-chip ok';
      chip.textContent = relay.session_id === mySessionId ? 'relaying via this tab' : `relaying via ${relay.label || 'another session'}`;
      const detail = document.createElement('span');
      detail.className = 'secret';
      detail.textContent = 'the daemon sends requests here; the key never leaves the relaying browser';
      row.append(lbl, chip, detail);
      list.appendChild(row);
    }
    card.appendChild(list);
  }

  const toggles = document.createElement('div');
  toggles.className = 'vault-actions';
  for (const kind of relayable) {
    const provider = kind.slice(8);
    const toggleId = `vault-egress-toggle-${provider}`;
    const wrap = document.createElement('span');
    wrap.className = 'vault-note';
    const toggle = document.createElement('input');
    toggle.type = 'checkbox';
    toggle.id = toggleId;
    toggle.checked = vaultEgressState.enabled.has(kind);
    toggle.addEventListener('change', () => {
      if (toggle.checked) vaultEgressState.enabled.add(kind);
      else vaultEgressState.enabled.delete(kind);
      vaultEgressPersistEnabled();
      vaultEgressEnsure()
        .then(() => vaultRefreshLeases({ force: true }))
        .catch(() => {});
    });
    const label = document.createElement('label');
    label.htmlFor = toggleId;
    label.textContent = ` Relay ${vaultProviderLabel(provider)} via this browser`;
    wrap.append(toggle, label);
    toggles.appendChild(wrap);
  }
  if (relayable.length) {
    toggles.appendChild(
      vaultButton('Test relay', async () => {
        const kind = relayable.find(k => vaultEgressState.enabled.has(k)) || relayable[0];
        vaultLeaseState.lastError = '';
        try {
          const result = await vaultLeaseRpc('api_credential_egress_probe', { kind });
          vaultLeaseState.lastError = `Relay test OK (${kind}): ${String(result?.text || '').slice(0, 120)}`;
        } catch (err) {
          vaultLeaseState.lastError = `Relay test failed: ${err?.message || err}`;
        }
        renderAccessVaultSection();
      })
    );
  }
  card.appendChild(toggles);
  if (vaultEgressState.lastError) {
    const error = document.createElement('div');
    error.className = 'vault-error';
    error.textContent = `Egress: ${vaultEgressState.lastError}`;
    card.appendChild(error);
  }
}

function vaultRenderUnlocked(card) {
  vaultRenderEntries(card);
  vaultRenderAddForm(card);
  vaultRenderFueling(card);
  vaultRenderUnlockers(card);

  const actions = document.createElement('div');
  actions.className = 'vault-actions';
  if (!vaultState.matchedEnvelopeId) {
    actions.appendChild(vaultButton('Enroll this passkey', () => vaultEnrollThisPasskey(), { primary: true }));
    const hint = document.createElement('span');
    hint.className = 'vault-note';
    hint.textContent = 'The passkey from this session cannot open the vault yet — enrolling adds an envelope, nothing is re-encrypted.';
    actions.appendChild(hint);
  }
  actions.appendChild(vaultButton('Lock vault', () => vaultLock()));
  card.appendChild(actions);

  if (vaultState.migratedVoiceKeys) {
    const note = document.createElement('div');
    note.className = 'vault-note';
    note.textContent = 'Voice API keys moved into the vault; the old per-origin browser copies were removed.';
    card.appendChild(note);
  }
}

function renderAccessVaultSection() {
  const mount = document.getElementById('access-vault-section');
  if (!mount) return;
  // Background renders must not clobber a phrase or key mid-typing.
  if (mount.contains(document.activeElement) && document.activeElement.matches('input, textarea, select')) return;

  // The custody trail lives directly under this section and shares its
  // refresh cadence (30s freshness guard inside).
  renderAccessCustodySection();
  vaultRefreshCustody().catch(() => {});
  // So do the agent sign-in cards (their own mount + freshness guards).
  renderAgentSigninSection();
  agentSigninRefresh('claude').catch(() => {});
  agentSigninRefresh('codex').catch(() => {});

  const card = document.createElement('div');
  card.className = 'vault-card';

  const statusLine = document.createElement('div');
  statusLine.className = 'vault-status-line';
  const chip = document.createElement('span');
  chip.className = 'vault-chip';
  let statusText = '';
  switch (vaultState.status) {
    case 'unavailable':
      chip.textContent = 'unavailable';
      statusText = !crypto?.subtle
        ? 'This browser lacks the WebCrypto features the vault needs.'
        : DASHBOARD_CONNECT_MODE
          ? 'The hosted vault store is unreachable right now.'
          : vaultDaemonStoreUnavailableText();
      break;
    case 'signed-out':
      chip.textContent = 'signed out';
      statusText = 'Sign in to the hosted account to reach your credential vault.';
      break;
    case 'none':
      chip.textContent = 'not created';
      statusText = 'No vault yet. Create one to keep provider credentials off every daemon disk — daemons will borrow time-boxed leases instead.';
      break;
    case 'locked':
      chip.textContent = 'locked';
      chip.classList.add('warn');
      statusText = `Revision ${vaultState.revision}, ${(vaultState.blob?.envelopes || []).length} unlocker(s). Unseal it with a passkey or the recovery phrase.`;
      break;
    case 'unlocked':
      chip.textContent = 'unlocked';
      chip.classList.add('ok');
      statusText = `Revision ${vaultState.revision}, ${vaultState.entries.length} credential(s). Unsealed in this tab's memory only.`;
      break;
    default:
      chip.textContent = 'checking';
      statusText = 'Looking for your vault…';
  }
  // Which store backs this vault — the one-glance answer to "where does
  // this blob live?" (docs/src/credential-custody.md, storage backends).
  const backend = vaultBackendKind();
  if (backend && vaultState.status !== 'unavailable') {
    const store = document.createElement('span');
    store.className = 'vault-chip';
    store.textContent = backend === 'daemon' ? 'stored on this daemon' : 'account store';
    store.title = backend === 'daemon'
      ? 'The sealed blob lives on this daemon (~/.intendant/vault-blob.json) — no Connect service in the loop. The daemon cannot read or forge it.'
      : 'The sealed blob lives with your hosted account and follows you across daemons. The service cannot read or forge it.';
    statusLine.appendChild(store);
  }
  statusLine.append(chip, document.createTextNode(statusText));
  card.appendChild(statusLine);

  // The backend can appear after boot (control channel connects, feature
  // list lands): leave 'unavailable' as soon as a store is reachable.
  if (vaultState.status === 'unavailable' && vaultAvailable()) {
    vaultInitPromise = null;
    vaultInit();
  }

  if (vaultState.rollbackWarning) {
    const warning = document.createElement('div');
    warning.className = 'vault-warning';
    warning.textContent = vaultState.rollbackWarning;
    card.appendChild(warning);
  }
  if (vaultState.lastError) {
    const error = document.createElement('div');
    error.className = 'vault-error';
    error.textContent = vaultState.lastError;
    card.appendChild(error);
  }

  // Hosted tab + unlocked vault + a daemon that has local vault storage:
  // offer to keep a sealed copy there, so its direct dashboard has a
  // vault home without any Connect service in the loop. Explicit and
  // one-way — the two stores keep independent revision ratchets.
  if (
    vaultState.status === 'unlocked' &&
    backend === 'account' &&
    vaultState.blob &&
    vaultTunnelDaemonVaultAvailable()
  ) {
    const copyRow = document.createElement('div');
    copyRow.className = 'vault-actions';
    const copyBtn = vaultButton('Keep a sealed copy on this daemon', async () => {
      try {
        const result = await vaultLeaseRpc('api_daemon_vault_publish', {
          revision: vaultState.blob.revision,
          vault: vaultState.blob,
        });
        showControlToast?.('success', result?.stored
          ? `Sealed vault copy (revision ${vaultState.blob.revision}) stored on this daemon — its direct dashboard can now unseal it with your passkey or phrase.`
          : 'This daemon already holds this exact vault revision.');
      } catch (err) {
        showControlToast?.('error', `Vault copy failed: ${err?.message || err}`);
      }
    });
    copyBtn.title = 'Publishes the encrypted blob to this daemon (~/.intendant/vault-blob.json). The daemon cannot read it; a direct dashboard on that machine unseals it with the same passkey or recovery phrase. The copy does not auto-sync afterwards.';
    copyRow.appendChild(copyBtn);
    card.appendChild(copyRow);
  }

  if (vaultCeremony) {
    vaultRenderCeremony(card);
  } else if (vaultState.status === 'none') {
    const actions = document.createElement('div');
    actions.className = 'vault-actions';
    actions.appendChild(
      vaultButton('Create vault', async () => {
        vaultCeremony = { phrase: await vaultGeneratePhrase() };
        vaultState.lastError = '';
        renderAccessVaultSection();
      }, { primary: true })
    );
    card.appendChild(actions);
  } else if (vaultState.status === 'locked') {
    vaultRenderLocked(card);
  } else if (vaultState.status === 'unlocked') {
    vaultRenderUnlocked(card);
  }

  mount.innerHTML = '';
  mount.appendChild(card);
}

/* ── ui-v2: Vault as its own top-level destination ──
   Under the ui-v2 flag, boot moves the vault + custody sections (and the
   two acc-section-heads introducing them) out of Access → Advanced into
   the #tab-vault pane. The nodes are RE-PARENTED, never rebuilt: every id,
   attached listener, and the window.intendantVault contract survives, and
   all renderers keep finding their mounts by id (the same relocation
   pattern the display canvases use). Under v1 (no flag) nothing runs, so
   the vault validators' #access/advanced contract stays byte-intact.
   Skipped on the standalone /access admin page, which is locked to the
   Access tab — there the vault stays reachable inside Advanced. A small
   link card is left behind so #access/advanced deep links (unfueled
   empty-state, older bookmarks) still lead somewhere honest. */
function ui2VaultAdoptSections() {
  if (DASHBOARD_ACCESS_PAGE_MODE) return;
  const host = document.getElementById('vault-tab-sections');
  const vaultMount = document.getElementById('access-vault-section');
  const custodyMount = document.getElementById('access-custody-section');
  if (!host || !vaultMount || !custodyMount) return;
  const advancedBody = vaultMount.parentElement;
  const vaultHead = vaultMount.previousElementSibling;
  const signinMount = document.getElementById('agent-signin-section');
  const signinHead = signinMount?.previousElementSibling;
  const custodyHead = custodyMount.previousElementSibling;
  if (vaultHead?.classList.contains('acc-section-head')) host.appendChild(vaultHead);
  host.appendChild(vaultMount);
  if (signinMount) {
    if (signinHead?.classList.contains('acc-section-head')) host.appendChild(signinHead);
    host.appendChild(signinMount);
  }
  if (custodyHead?.classList.contains('acc-section-head')) host.appendChild(custodyHead);
  host.appendChild(custodyMount);
  const pagehead = document.getElementById('ui2-vault-pagehead');
  if (pagehead) pagehead.hidden = false;
  const v1Note = document.getElementById('vault-v1-note');
  if (v1Note) v1Note.hidden = true;
  if (advancedBody) {
    const moved = document.createElement('div');
    moved.className = 'ui2-vault-moved-card';
    const title = document.createElement('div');
    title.className = 'ui2-vault-moved-title';
    title.textContent = 'The vault has its own home now';
    const sub = document.createElement('div');
    sub.className = 'ui2-vault-moved-sub';
    sub.textContent = 'Credential vault, fueling, and the custody trail moved to Vault in the navigation rail.';
    const go = document.createElement('button');
    go.type = 'button';
    go.className = 'ui2-vault-moved-go';
    go.textContent = 'Open Vault';
    go.addEventListener('click', () => routeTo('vault'));
    moved.append(title, sub, go);
    advancedBody.prepend(moved);
  }
}
ui2VaultAdoptSections();

/* Debug/validator handle (the module scope hides the vault globals; the
   validators observe state through this, like intendantDashboardControl). */
window.intendantVault = {
  state: () => ({
    status: vaultState.status,
    revision: vaultState.revision,
    highWater: vaultState.highWater,
    entries: vaultState.entries.map(e => ({ ...e })),
    envelopes: (vaultState.blob?.envelopes || []).map(e => ({ kind: e.kind, id: e.id, label: e.label })),
    matchedEnvelopeId: vaultState.matchedEnvelopeId,
    rollbackWarning: vaultState.rollbackWarning,
    lastError: vaultState.lastError,
    migratedVoiceKeys: vaultState.migratedVoiceKeys,
  }),
  init: () => vaultInit(),
  lock: () => vaultLock(),
  unlockWithPasskey: () => vaultUnlockWithPasskey(),
  unlockWithPhrase: phrase => vaultUnlockWithPhrase(phrase),
  voiceApiKeyGet: storageKey => voiceApiKeyGet(storageKey),
  leases: () => ({
    supported: vaultLeaseState.supported,
    availability: vaultLeaseState.availability,
    leases: vaultLeaseState.leases.map(lease => ({ ...lease })),
    expiredNote: vaultLeaseState.expiredNote,
    lastError: vaultLeaseState.lastError,
    ownLeaseIds: Array.from(vaultOwnLeaseIds),
  }),
  refreshLeases: () => vaultRefreshLeases({ force: true }),
  fuelEntry: entryId => {
    const entry = vaultState.entries.find(e => e.id === entryId);
    if (!entry) return Promise.reject(new Error('no such vault entry'));
    return vaultFuelEntry(entry);
  },
  revokeLease: leaseId => vaultRevokeLease(leaseId),
  /* Validator-only: point OAuth refresh at a mock token endpoint, and
     drive one renewal tick on demand instead of waiting five minutes. */
  setOauthEndpoints: (map = {}) => {
    for (const key of Object.keys(vaultOauthEndpointOverrides)) delete vaultOauthEndpointOverrides[key];
    Object.assign(vaultOauthEndpointOverrides, map);
  },
  renewTick: () => vaultRenewOwnLeasesOnce(),
  setOauthLeases: enabled => vaultSetOauthLeasesEnabled(Boolean(enabled)),
  setEgress: (kinds, opts = {}) => {
    vaultEgressState.enabled = new Set((Array.isArray(kinds) ? kinds : []).map(String));
    vaultEgressPersistEnabled();
    if (opts.allowHosts && typeof opts.allowHosts === 'object') {
      vaultEgressState.allowHosts = { ...opts.allowHosts };
    }
    return vaultEgressEnsure().then(() => vaultRefreshLeases({ force: true }));
  },
  egress: () => ({
    enabled: Array.from(vaultEgressState.enabled),
    registered: Array.from(vaultEgressState.registered),
    relays: (vaultLeaseState.egress || []).map(relay => ({ ...relay })),
    lastError: vaultEgressState.lastError,
    jobs: vaultEgressJobs.size,
  }),
  probeEgress: kind => vaultLeaseRpc('api_credential_egress_probe', { kind }),
  custody: () => ({
    supported: custodyTrailState.supported,
    availability: custodyTrailState.availability,
    events: custodyTrailState.events.map(event => ({ ...event })),
  }),
  refreshCustody: () => vaultRefreshCustody({ force: true }),
};

/* Sign an offer. Absence is legacy-compatible (the daemon falls back to
   account/trusted-transport identity); a present-but-invalid signature is
   rejected by the daemon, so a signaling relay can neither forge nor splice
   a key binding.

   With `account` ({userId, name}) the signature covers the v2 payload,
   which additionally binds this browser's OWN account claim — the pending
   enrollment then shows "@name" attested by the device key instead of
   whatever the relay asserts. Mirrors access/client_key.rs payloads
   byte-for-byte. */
async function clientIdentityOfferFields(daemonId, clientNonce, sdp, account) {
  try {
    const identity = await clientIdentityGet();
    if (!identity) return {};
    const ts = Date.now();
    const sdpDigest = dashboardBytesToBase64Url(
      new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(sdp)))
    );
    const accountUserId = account?.userId ? String(account.userId).trim() : '';
    const accountName = account?.name ? String(account.name).trim() : '';
    const v2 = Boolean(accountUserId);
    const payload = new TextEncoder().encode(
      v2
        ? `intendant-client-key-offer-v2\n${daemonId || ''}\n${clientNonce || ''}\n${sdpDigest}\n${ts}\n${accountUserId}\n${accountName}`
        : `intendant-client-key-offer-v1\n${daemonId || ''}\n${clientNonce || ''}\n${sdpDigest}\n${ts}`
    );
    const signature = await crypto.subtle.sign(
      { name: 'ECDSA', hash: 'SHA-256' },
      identity.privateKey,
      payload
    );
    return {
      client_key: identity.publicRawB64u,
      client_key_sig: dashboardBytesToBase64Url(new Uint8Array(signature)),
      client_key_ts: ts,
      ...(v2
        ? {
            client_key_proto: 'intendant-client-key-offer-v2',
            client_key_account_user_id: accountUserId,
            ...(accountName ? { client_key_account_name: accountName } : {}),
          }
        : {}),
    };
  } catch (err) {
    console.warn('[client-identity] offer signing failed:', err?.message || err);
    return {};
  }
}

/* ── Stored org grant documents ──
   Signed org-grant documents this browser holds, keyed by org handle
   (trust architecture phase 6). A document pasted into the join fold is
   kept here so later offers can carry it: first contact with any daemon
   that trusts the org then materializes the grant and connects in one
   round trip. Storage is convenience, not authority — every daemon
   re-verifies the signature, expiry, and its own org trust on each
   presentation, and only the bound subject key benefits. */

const ORG_GRANTS_STORE_KEY = 'intendant_org_grants_v1';

function orgGrantDocValid(doc) {
  return Boolean(
    doc && typeof doc === 'object' && !Array.isArray(doc) &&
    doc.v === 1 && doc.kind === 'org-grant' &&
    typeof doc.org?.handle === 'string' && doc.org.handle &&
    typeof doc.subject?.client_key_fingerprint === 'string' &&
    typeof doc.sig === 'string' && doc.sig &&
    Number(doc.expires_at_unix_ms) > 0
  );
}

/* Read the stored documents ({handle: doc}), pruning expired or malformed
   entries as they age out. */
function orgGrantsRead() {
  let map = {};
  try { map = JSON.parse(localStorage.getItem(ORG_GRANTS_STORE_KEY) || '{}'); } catch {}
  if (!map || typeof map !== 'object' || Array.isArray(map)) map = {};
  const now = Date.now();
  let dirty = false;
  for (const [handle, doc] of Object.entries(map)) {
    if (!orgGrantDocValid(doc) || Number(doc.expires_at_unix_ms) <= now) {
      delete map[handle];
      dirty = true;
    }
  }
  if (dirty) {
    try { localStorage.setItem(ORG_GRANTS_STORE_KEY, JSON.stringify(map)); } catch {}
  }
  return map;
}

/* Keep a document for automatic presentation on future offers. Returns
   whether it was stored (valid shape, not yet expired). */
function orgGrantStore(doc) {
  if (!orgGrantDocValid(doc) || Number(doc.expires_at_unix_ms) <= Date.now()) return false;
  try {
    const map = orgGrantsRead();
    map[doc.org.handle] = doc;
    localStorage.setItem(ORG_GRANTS_STORE_KEY, JSON.stringify(map));
    return true;
  } catch {
    return false;
  }
}

/* The stored document to ride along on an offer to `daemonId` (pass '' on
   the local same-origin path, where the daemon id is unknown client-side):
   bound to THIS browser's identity key, unexpired, and targeted at the
   daemon when its id is known. Latest issued wins when several match —
   the daemon target-checks again either way. */
async function orgGrantForOffer(daemonId) {
  try {
    const identity = await clientIdentityGet();
    if (!identity) return null;
    const now = Date.now();
    let best = null;
    for (const doc of Object.values(orgGrantsRead())) {
      if (String(doc.subject?.client_key_fingerprint || '').trim() !== identity.fingerprint) continue;
      if (Number(doc.expires_at_unix_ms) <= now) continue;
      const targets = Array.isArray(doc.targets) ? doc.targets.map(t => String(t).trim()) : [];
      if (daemonId && targets.length && !targets.includes('*') && !targets.includes(daemonId)) continue;
      if (!best || Number(doc.issued_at_unix_ms || 0) > Number(best.issued_at_unix_ms || 0)) best = doc;
    }
    return best;
  } catch {
    return null;
  }
}

/* The rendezvous origin this page can reach for org bulletin lookups:
   the current origin in hosted-connect mode, else the daemon-advertised
   base carried in the URL. */
function dashboardRendezvousBase() {
  if (DASHBOARD_CONNECT_MODE) return DASHBOARD_CONNECT_SIGNALING_BASE || window.location.origin;
  if (DASHBOARD_CONNECT_SIGNALING_BASE) return DASHBOARD_CONNECT_SIGNALING_BASE;
  // Anchor pages: the daemon advertises the rendezvous it polls in its
  // own dashboard targets (phase 7).
  try {
    const self = (dashboardAccessTargets || []).find(target => target?.local === true && target?.rendezvous_base);
    return String(self?.rendezvous_base || '').trim().replace(/\/+$/, '');
  } catch { return ''; }
}

/* Best-effort publish of a root-signed revocation list to the rendezvous
   bulletin board, so member browsers everywhere pick it up. Zero
   authority: the board verifies the signature only to stay clean, and
   every daemon re-verifies on apply. */
async function orgPublishRevocations(orl) {
  const base = dashboardRendezvousBase();
  if (!base || !orl) return false;
  try {
    const resp = await fetch(`${base}/api/orgs/revocations/publish`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(orl),
    });
    return resp.ok;
  } catch (err) {
    console.warn('[org] revocation publish failed:', err?.message || err);
    return false;
  }
}

/* Member-side courier: for each stored org document, fetch the org's
   published revocation list from the rendezvous and hand it to the
   daemon this page is connected to. The daemon enforces signature and
   monotonic seq itself; an already-applied list is a quiet no-op. */
async function orgRevocationCourier() {
  const base = dashboardRendezvousBase();
  if (!base) return;
  let map = {};
  try { map = JSON.parse(localStorage.getItem(ORG_GRANTS_STORE_KEY) || '{}') || {}; } catch {}
  for (const doc of Object.values(map)) {
    const handle = String(doc?.org?.handle || '').trim();
    const rootKey = String(doc?.org?.root_key || '').trim();
    if (!handle || !rootKey) continue;
    const throttleKey = `intendant_orl_courier:${handle}:${rootKey}`;
    try {
      const last = Number(sessionStorage.getItem(throttleKey) || 0);
      if (Date.now() - last < 30 * 60 * 1000) continue;
      sessionStorage.setItem(throttleKey, String(Date.now()));
    } catch {}
    try {
      const resp = await fetch(`${base}/api/orgs/revocations?handle=${encodeURIComponent(handle)}&root_key=${encodeURIComponent(rootKey)}`);
      if (!resp.ok) continue;
      const body = await resp.json().catch(() => ({}));
      if (!body?.orl) continue;
      const applied = await accessOrgCall('api_access_org_orl_apply', body.orl);
      if (applied?.applied?.changed) {
        console.info(`[org] carried @${handle} revocations seq ${applied.applied.seq} to this daemon (${applied.applied.revoked_grants} grants, ${applied.applied.revoked_peer_identities} peer identities revoked)`);
      }
    } catch (err) {
      console.warn(`[org] revocation courier for @${handle} failed:`, err?.message || err);
    }
  }
}

function dashboardConnectMutationUnavailable(action, label = 'Dashboard control request', options = {}) {
  const actionLabel = action ? ` ${action}` : '';
  const message = `${label}${actionLabel} is unavailable until dashboard access is ready`;
  console.warn(`[dashboard-control] ${message}`);
  const err = new Error(message);
  if (typeof options.onError === 'function') {
    options.onError(err);
  } else if (typeof showControlToast === 'function') {
    showControlToast('error', message);
  }
  return false;
}

// F8a warn-logging shim: zero in-repo callers after the flip — every
// product call site rides the daemonApi facade. The legacy body stays
// verbatim for the soak week (delegation double-records under
// 'dashboardJsonFetch' and 'jsonFetch' by design — the label names the
// missed method either way); window.qa.transportShimHits() is the soak
// verdict, and F8b deletes this with the DashboardTransport shims.
function dashboardJsonFetch(method, params, fallback, label = method, options = {}) {
  dashboardTransportShimRecord('dashboardJsonFetch', label);
  if (dashboardTransport && typeof dashboardTransport.jsonFetch === 'function') {
    return dashboardTransport.jsonFetch(method, params, fallback, label, options);
  }
  return fallback();
}

function dispatchSessionControlMsg(payload, options = {}) {
  payload = hostedControlNormalizeControlMessage(payload);
  if (!payload) {
    const error = new Error('That action is outside this hosted lease preset');
    if (typeof options.onError === 'function') options.onError(error);
    else if (typeof showControlToast === 'function') showControlToast('error', error.message);
    return false;
  }
  const action = String(payload?.action || '').trim();
  const fallback = () => {
    if (dashboardConnectModeEnabled()) {
      return dashboardConnectMutationUnavailable(action, 'Session control request', options);
    }
    if (app && app.send_server_action) {
      // send_server_action reports whether the frame reached an OPEN
      // legacy /ws socket (only an explicit false refuses — QA stubs
      // return undefined). A refused intent must not die silently: kick
      // the event-lane fallback so the tunnel comes up, and tell the
      // caller the send never left the browser.
      if (app.send_server_action(payload) !== false) return true;
      if (typeof dashboardTriggerEventLaneFallback === 'function') {
        dashboardTriggerEventLaneFallback(`${action || 'session control'} intent found no open event lane`);
      }
      console.warn('session control: no open event lane, refused', action || payload);
      const err = new Error('Dashboard control connection is down — reconnecting. Retry in a moment.');
      if (typeof options.onError === 'function') options.onError(err);
      else if (typeof showControlToast === 'function') showControlToast('error', err.message);
      return false;
    }
    console.warn('session control: no app connection, dropped', payload);
    return false;
  };
  if (!DASHBOARD_SESSION_CONTROL_MSG_RPC_ACTIONS.has(action)) return fallback();
  // Transport F7: the RPC leg rides the daemonApi facade. WS-twin residue
  // method (no HTTP row by design — the /ws intent stream below is the
  // twin), so no HTTP lane exists for it; the availability derivation
  // replaces the hand-rolled canUseRpc + status-boolean probe, and denied
  // or too-old daemons fall through to /ws exactly as the strict boolean
  // did. A tunnel attempt is final: never replayed over /ws.
  if (daemonApi.availability('api_session_control_msg').ok) {
    daemonApi.request('api_session_control_msg', { message: payload }, {
      timeoutMs: options.timeoutMs || 15000,
    }).then(resp => {
      if (resp.ok) return;
      // Delivered refusals (allowlist drift the parity pins make
      // near-impossible) surface instead of silently resolving; still no
      // /ws replay — the response was delivered.
      const err = new Error(resp.body?.error || 'Dashboard control request failed');
      console.warn(`[dashboard-control] ${action} session ControlMsg RPC refused; not replaying over /ws`, resp.body?.error || resp.status);
      if (typeof options.onError === 'function') {
        options.onError(err);
      } else if (typeof showControlToast === 'function') {
        showControlToast('error', err.message);
      }
    }).catch(err => {
      console.warn(`[dashboard-control] ${action} session ControlMsg RPC failed; not replaying over /ws`, err);
      if (typeof options.onError === 'function') {
        options.onError(err);
      } else if (typeof showControlToast === 'function') {
        showControlToast('error', err?.message || 'Dashboard control request failed');
      }
    });
    return true;
  }
  return fallback();
}

function dispatchDashboardActionMsg(payload, options = {}) {
  const action = String(payload?.action || '').trim();
  const fallback = () => {
    if (dashboardConnectModeEnabled()) {
      return dashboardConnectMutationUnavailable(action, 'Dashboard action request', options);
    }
    if (app && app.send_server_action) {
      // Same refused-send contract as dispatchSessionControlMsg above.
      if (app.send_server_action(payload) !== false) return true;
      if (typeof dashboardTriggerEventLaneFallback === 'function') {
        dashboardTriggerEventLaneFallback(`${action || 'dashboard action'} intent found no open event lane`);
      }
      console.warn('dashboard action: no open event lane, refused', action || payload);
      const err = new Error('Dashboard control connection is down — reconnecting. Retry in a moment.');
      if (typeof options.onError === 'function') options.onError(err);
      else if (typeof showControlToast === 'function') showControlToast('error', err.message);
      return false;
    }
    console.warn('dashboard action: no app connection, dropped', payload);
    return false;
  };
  if (!DASHBOARD_ACTION_MSG_RPC_ACTIONS.has(action)) return fallback();
  // Transport F7: same facade + WS-twin residue shape as
  // dispatchSessionControlMsg above — no HTTP lane, availability-derived
  // routing, tunnel attempts never replayed over /ws.
  if (daemonApi.availability('api_dashboard_action_msg').ok) {
    daemonApi.request('api_dashboard_action_msg', { message: payload }, {
      timeoutMs: options.timeoutMs || 15000,
    }).then(resp => {
      if (resp.ok) return;
      const err = new Error(resp.body?.error || 'Dashboard action failed');
      console.warn(`[dashboard-control] ${action} dashboard action RPC refused; not replaying over /ws`, resp.body?.error || resp.status);
      if (typeof options.onError === 'function') {
        options.onError(err);
      } else if (typeof showControlToast === 'function') {
        showControlToast('error', err.message);
      }
    }).catch(err => {
      console.warn(`[dashboard-control] ${action} dashboard action RPC failed; not replaying over /ws`, err);
      if (typeof options.onError === 'function') {
        options.onError(err);
      } else if (typeof showControlToast === 'function') {
        showControlToast('error', err?.message || 'Dashboard action failed');
      }
    });
    return true;
  }
  return fallback();
}

// Tracks which peer rows have their per-peer controls panel expanded.
// Lives outside renderDaemonsList because the list re-renders on every
// push event (PeerAdded / PeerRemoved / PeerStateChanged) and the
// expand state needs to survive those re-renders — otherwise the panel
// collapses every time a peer's status updates, which is broken UX.
const expandedDaemons = new Set();

// Per-peer pending approvals: host_id → Map<approval_id_str, {command, category}>.
// Populated when a secondary peer emits an `approval_required` event;
// drained when the user resolves the approval (POST /api/peers/{id}/
// approval) or when the peer emits `approval_resolved`. Survives
// dashboard re-renders the same way `expandedDaemons` does.
const peerPendingApprovals = new Map();

// (Storage key constants are declared earlier, above activeHostFilter,
// to avoid a temporal-dead-zone reference error.)

// Standalone shell (lazy) — separate xterm instance in the Terminal tab's
// "Shell" sub-tab. Keyed by (hostId, terminalId) so the same state shape
// covers the future multi-host case.
let shellTerm = null;
let shellFitAddon = null;
let shellInitialized = false;
let shellOpenSent = false;
let shellOpenAcked = false;
// Sharing state of the current shell session (from terminal_opened /
// terminal_shared acks). can_share is true for the session owner or root.
let shellShared = false;
let shellCanShare = false;
const SHELL_HOST_ID = 'local';
const SHELL_TERMINAL_ID = 'shell-0';
let selectedShellHostId = SHELL_HOST_ID;
let shellOutputQueue = [];
let shellOutputQueuedBytes = 0;
let shellOutputFlushScheduled = false;
let shellQueuedInput = '';
let shellWaitingNoticeShown = false;
let shellPendingResize = null;
const SHELL_QUEUED_INPUT_MAX_BYTES = 64 * 1024;
let activeTermSubtab = 'shell';

// Files editor state. Declared here, far above the editor's functions,
// because a #files deep link applies the route synchronously while the
// module is still evaluating — the first filesIdeOnTabShown call must
// find this state already initialized (TDZ otherwise aborts the module).
const FILES_IDE_MAX_EDIT_BYTES = 2 * 1024 * 1024;
const FILES_IDE_MAX_TABS = 20;
const VALID_FILES_SUBTABS = ['editor', 'transfers'];
const filesIdeBuffers = new Map(); // key -> buffer
const filesIdeTreeStates = new Map(); // hostId -> tree state
let filesIdeActiveKey = '';
let filesIdeCm = null;
let filesIdeLibPromise = null;
let filesIdeInitialized = false;
let activeFilesSubtab = 'editor';
// Find-in-file state (single bar over the active buffer).
let filesIdeFindOpen = false;
let filesIdeFindMatches = []; // [{from, to}]
let filesIdeFindMarks = []; // CodeMirror TextMarker handles
let filesIdeFindIndex = -1;
let filesIdeFindTimer = 0;

// Shell key bar state: sticky modifiers (Ctrl, Alt) that transform the
// next character typed on the soft keyboard. Cleared after one use.
const shellModifiers = { ctrl: false, alt: false };

// Lookup table for shell key bar buttons. Keys here map to raw byte
// sequences sent straight to the PTY. Defined in JS rather than in HTML
// `data-seq` attributes because HTML attributes don't interpret
// JavaScript escape sequences — `data-seq="\u001b"` would send the
// literal 6 characters `\u001b` to the shell, not an ESC byte.
const SHELL_KEY_SEQS = {
  esc: '\x1b',
  tab: '\t',
  // ^C / ^D / ⏎ chips (ui-v2 design bar; buttons are hidden under v1).
  ctrlc: '\x03',
  ctrld: '\x04',
  enter: '\r',
  up: '\x1b[A',
  down: '\x1b[B',
  left: '\x1b[D',
  right: '\x1b[C',
  home: '\x1b[H',
  end: '\x1b[F',
  pgup: '\x1b[5~',
  pgdn: '\x1b[6~',
  del: '\x1b[3~',
  pipe: '|',
  slash: '/',
  backslash: '\\',
  tilde: '~',
  dash: '-',
  dollar: '$',
  star: '*',
};

// Displays
const displaySlots = new Map();
const peerDisplayConnections = new Map(); // sessionKey -> PeerDisplayConnection
// Per-display agent visibility, fed by `display_ready` / `user_display_granted`
// events (both carry `agent_visible` since the private-view split). false =
// a private user view ("View this machine"): streams to this dashboard only,
// invisible to agent screenshot/CU/enumeration paths on the daemon.
const displayAgentVisibility = new Map();
// Display ids that are user-display sessions (granted or private-viewed) --
// distinguishes "agent can see this" chips on user screens from ordinary
// agent-owned virtual displays, which get no chip.
const userDisplayIds = new Set();
// User-display grant state (single active slot, matching the daemon's
// per-daemon grant flag). Declared here -- before the display fragments
// that render against it at load time -- because module-scope `let` from
// a later fragment is in TDZ during earlier fragments' initial render.
let userDisplayGranted = false;
let grantedDisplayId = 0;
// Mode of the active user-display session: true = shared with the agent
// (computer use), false = a private view. Meaningful only while
// userDisplayGranted is true.
let userDisplayAgentVisible = true;
function setDisplayAgentVisibility(displayId, visible) {
  displayId = Number(displayId);
  displayAgentVisibility.set(displayId, !!visible);
  const slot = displaySlots.get(displayId);
  if (slot && typeof slot.setAgentVisibility === 'function') {
    slot.setAgentVisibility(!!visible);
  }
}
function clearDisplayAgentVisibility(displayId) {
  displayId = Number(displayId);
  displayAgentVisibility.delete(displayId);
  userDisplayIds.delete(displayId);
}
// Session/phase state is read by Station during direct #station boot before
// the lower dashboard sections finish registering their helpers.
const sessionsListCache = new Map();
const sessionsListInflight = new Map();
const SESSION_HYDRATION_DONE_HIDE_MS = 900;
let sessionsRecentLimit = SESSION_LIST_RECENT_LIMIT;
// '' = this daemon; a peer host_id while browsing that peer's sessions
// from the Sessions tab host strip.
let sessionsActiveHostId = '';

function applyGatewayConfig(config) {
  const cfg = config && typeof config === 'object' ? config : {};
  gatewayConfig = cfg;
  if (currentExternalAgent === null) {
    currentExternalAgent = normalizeAgentId(cfg.external_agent);
  }
  // Every lane's config lands here (boot fetch, tunnel config RPC,
  // reconnect hydration, /ws-reconnect refetch) — the one chokepoint where
  // a stale tab learns the daemon now serves a newer bundle.
  if (typeof maybeNudgeStaleBuild === 'function') maybeNudgeStaleBuild(cfg.app_build);
  applyMainBackendStatus();
}

function applyAgentCardIdentity(card) {
  if (!card || !card.id || !card.label) return false;
  const previousSelfPeerId = selfPeerId;
  // Stable host identity: shown in the status bar so the user
  // always knows which daemon the dashboard is primarily talking
  // to. The card's `label` is the display name; `id` is the full
  // PeerId (e.g. "intendant:nicks-mac") used as the routing key
  // throughout the dashboard so two peers with the same label
  // stay distinct and a renamed daemon keeps the same routing
  // stability.
  selfPeerId = card.id;
  if (previousSelfPeerId && previousSelfPeerId !== selfPeerId) {
    const earlyUsage = hostStatsCache.get(previousSelfPeerId);
    if (earlyUsage && !hostStatsCache.has(selfPeerId)) {
      hostStatsCache.set(selfPeerId, earlyUsage);
    }
    for (const [key, earlySessions] of Array.from(sessionsListCache.entries())) {
      if (!key.startsWith(`${previousSelfPeerId}\u001f`)) continue;
      const newKey = `${selfPeerId}${key.slice(previousSelfPeerId.length)}`;
      if (!sessionsListCache.has(newKey)) sessionsListCache.set(newKey, earlySessions);
    }
    for (const [key, earlySessionsInflight] of Array.from(sessionsListInflight.entries())) {
      if (!key.startsWith(`${previousSelfPeerId}\u001f`)) continue;
      const newKey = `${selfPeerId}${key.slice(previousSelfPeerId.length)}`;
      if (!sessionsListInflight.has(newKey)) {
        sessionsListInflight.set(newKey, earlySessionsInflight);
      }
    }
  }
  selfHostLabel = card.label;
  const el = document.getElementById('sb-host-label');
  if (el) el.textContent = card.label;
  // Version + git SHA are used by Access targets to flag
  // version skew across multi-host connections.
  selfVersion = card.version || '';
  selfGitSha = card.git_sha || '';
  upsertDashboardAccessTarget({
    id: card.id,
    host_id: card.id,
    label: card.label,
    local: true,
    source: 'agent-card',
    access_domain: 'user_client',
    access_domain_label: 'User/client access',
    route: 'current_dashboard',
    route_label: 'Current dashboard',
    auth: 'trusted_dashboard',
    auth_label: 'Trusted dashboard session',
    effective_role: 'root',
    effective_role_label: 'Root',
    connected: true,
    capabilities: card.capabilities || [],
  });
  return true;
}

function sessionListRequestLimit(options = {}) {
  if (options.limit === 'all' || options.limit === 'full' || options.full === true) return 'all';
  const raw = options.limit ?? SESSION_LIST_RECENT_LIMIT;
  const n = Number(raw);
  return Number.isFinite(n) && n > 0 ? Math.min(Math.floor(n), 5000) : SESSION_LIST_RECENT_LIMIT;
}

function sessionListCacheKey(hostId, options = {}) {
  const view = options.view === 'usage' ? 'usage' : 'full';
  return `${hostId || selfPeerId}\u001f${sessionListRequestLimit(options)}\u001f${view}`;
}

function sessionListUrl(baseUrl, limit, view = '') {
  const prefix = baseUrl ? baseUrl.replace(/\/$/, '') : '';
  const value = limit === 'all' ? 'all' : String(limit);
  const suffix = view === 'usage' ? '&view=usage' : '';
  return `${prefix}/api/sessions?limit=${encodeURIComponent(value)}${suffix}`;
}

function sessionListStreamUrl(baseUrl, limit) {
  const prefix = baseUrl ? baseUrl.replace(/\/$/, '') : '';
  const value = limit === 'all' ? 'all' : String(limit);
  return `${prefix}/api/sessions/stream?limit=${encodeURIComponent(value)}`;
}

let worktreesLoaded = false;
let _cachedWorktreeScan = null;
let worktreesLoadInFlight = '';
const AGENT_ACTIVE_PHASES = new Set([
  'thinking', 'running', 'running_agent', 'orchestrating',
  'waiting_approval', 'waiting_human', 'interrupting',
]);
const sharedViewState = {
  visible: false,
  displayId: null,
  displayTarget: '',
  action: '',
  reason: '',
  note: '',
  region: null,
};
// Phase 5c: buffer for `display_input_authority_state` frames that
// arrive before the corresponding DisplaySlot exists (race between the
// `display_ready` and `display_input_authority_state` channels in the
// per-WS outbound select).  Keyed by display_id; values are the latest
// state string (`'you' | 'other' | 'unclaimed'`).  Drained in
// `addDisplaySlot` right after the slot is added to `displaySlots`.
// Replaced (not appended) on each frame so a stale state never wins
// over a fresh one if multiple arrive while the slot is missing.
const pendingAuthorityStates = new Map();

// Station tab bridge. Rendering and interaction live in Rust/WASM; JS only
// normalizes existing dashboard state and hands over live WebRTC video nodes.
let station = null;
let stationWasmReady = false;
let stationInitPromise = null;
let stationCurrentTask = '';
let stationCurrentApproval = null;
let stationCurrentHumanQuestion = '';
const stationLogEvents = [];
const STATION_ACTIVITY_EVENT_LIMIT = 180;
const stationLogAnchorRows = new Map();
const stationLogAnchorKeys = [];
const STATION_LOG_ANCHOR_LIMIT = 10000;
// Incremental per-session index over stationLogAnchorRows so managed-context
// summaries never rescan the full (10k-cap) anchor map per snapshot rebuild:
// distinct-id refcounts for counting plus a newest-first tail per session.
const stationAnchorsBySession = new Map(); // sid -> { ids: Set<id>, tail: [row, ...] oldest->newest }
const stationAnchorIdRefs = new Map(); // id -> number of sessions holding it
const STATION_ANCHOR_TAIL_LIMIT = 8;
let stationLogAnchorSeq = 0;
const stationRegisteredSources = new Set();
let stationLastSnapshotJson = '';
let stationSessionsIndexLoading = false;
let stationSessionsIndexError = '';
let stationWebgpuUnavailable = false;

function stationStatus(text) {
  // Transient status messages land in the text span; the sibling metrics
  // span (renderer · fps · displays) is owned by stationUpdateMetricsChip.
  const el = document.getElementById('station-status-text') || document.getElementById('station-status');
  if (!el) return;
  let value = String(text || '');
  if (stationWebgpuUnavailable && !value.includes('WebGPU unavailable')) {
    value = `${value ? `${value} — ` : ''}WebGPU unavailable; canvas renderer active. Trust the dashboard certificate (or use a WebGPU-capable browser) to enable WebGPU.`;
  }
  el.textContent = value;
}
