/* ── Credential vault (credential custody v1) ──
   The user's provider credentials, end-to-end encrypted client-side and
   synced blind through the hosted rendezvous (/api/vault). A random
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
   only here, in memory, behind a passkey gesture or the phrase. */

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
  kBytes: null,        // raw master key while unlocked (zeroed on lock)
  matchedEnvelopeId: null, // which prf envelope this session's passkey opened
  macSeen: false,      // downgrade ratchet: an authenticated blob has been seen
  rollbackWarning: '',
  lastError: '',
  migratedVoiceKeys: false,
};
let vaultCeremony = null;          // { phrase } while the create ceremony is on screen
let vaultPublishChain = Promise.resolve(true);
const vaultRevealedEntries = new Set();

function vaultAvailable() {
  return DASHBOARD_CONNECT_MODE && Boolean(crypto?.subtle);
}

/* ── Vault crypto ── */

async function vaultHkdfAesKey(secretBytes, info) {
  const hkdf = await crypto.subtle.importKey('raw', secretBytes, 'HKDF', false, ['deriveKey']);
  return crypto.subtle.deriveKey(
    {
      name: 'HKDF',
      hash: 'SHA-256',
      salt: new TextEncoder().encode(VAULT_HKDF_SALT),
      info: new TextEncoder().encode(info),
    },
    hkdf,
    { name: 'AES-GCM', length: 256 },
    false,
    ['encrypt', 'decrypt']
  );
}

/* KEK from the dedicated vault PRF domain — what new envelopes use. */
async function vaultPrfKekDedicated() {
  const prfB64u = sessionStorage.getItem(VAULT_PRF_SESSION_KEY) || '';
  if (!prfB64u) return null;
  try {
    return await vaultHkdfAesKey(dashboardBase64UrlToBytes(prfB64u), 'vault-kek');
  } catch (err) {
    console.warn('[vault] vault-PRF key derivation failed:', err?.message || err);
    return null;
  }
}

/* KEK from the fleet PRF secret — legacy: pre-two-salt envelopes were
   wrapped under this. Kept for unlocking them (and as the wrap fallback
   for authenticators that only evaluate one PRF salt). */
async function vaultPrfKekLegacy() {
  const prfB64u = sessionStorage.getItem(FLEET_PRF_SESSION_KEY) || '';
  if (!prfB64u) return null;
  try {
    return await vaultHkdfAesKey(dashboardBase64UrlToBytes(prfB64u), 'vault-kek');
  } catch (err) {
    console.warn('[vault] PRF key derivation failed:', err?.message || err);
    return null;
  }
}

/* KEK for wrapping a NEW envelope: [kek, marker]. Dedicated domain when
   the authenticator evaluated both salts, legacy (markerless) otherwise. */
async function vaultPrfKekForWrap() {
  const dedicated = await vaultPrfKekDedicated();
  if (dedicated) return [dedicated, VAULT_PRF_ENVELOPE_MARK];
  return [await vaultPrfKekLegacy(), null];
}

/* KEK for OPENING an existing envelope, chosen by its marker. */
async function vaultPrfKekForEnvelope(envelope) {
  return envelope?.prf === VAULT_PRF_ENVELOPE_MARK
    ? vaultPrfKekDedicated()
    : vaultPrfKekLegacy();
}

/* Standard BIP39 seed stretch (PBKDF2-HMAC-SHA512, salt 'mnemonic',
   2048 iterations — the 128-bit entropy does the security work), then
   our own HKDF domain down to an AES-GCM key. */
async function vaultPhraseKek(phrase) {
  const password = await crypto.subtle.importKey(
    'raw', new TextEncoder().encode(phrase.normalize('NFKD')), 'PBKDF2', false, ['deriveBits']
  );
  const seed = await crypto.subtle.deriveBits(
    { name: 'PBKDF2', hash: 'SHA-512', salt: new TextEncoder().encode('mnemonic'), iterations: 2048 },
    password,
    512
  );
  return vaultHkdfAesKey(new Uint8Array(seed), 'vault-kek-phrase');
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

function vaultEnvelopeAad() {
  return new TextEncoder().encode(`${VAULT_HKDF_SALT}|kek`);
}

/* The body AAD binds the revision into the ciphertext, so the store
   cannot re-label an old body with a new revision number; replaying a
   complete old blob (rollback) remains and is what highWater detects. */
function vaultBodyAad(revision) {
  return new TextEncoder().encode(`${VAULT_HKDF_SALT}|body|rev:${revision}`);
}

async function vaultWrapMasterKey(kek, kBytes) {
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const wrapped = await crypto.subtle.encrypt(
    { name: 'AES-GCM', iv, additionalData: vaultEnvelopeAad() },
    kek,
    kBytes
  );
  return {
    iv: dashboardBytesToBase64Url(iv),
    wrapped: dashboardBytesToBase64Url(new Uint8Array(wrapped)),
  };
}

async function vaultUnwrapMasterKey(kek, envelope) {
  try {
    const kBytes = await crypto.subtle.decrypt(
      { name: 'AES-GCM', iv: dashboardBase64UrlToBytes(String(envelope.iv || '')), additionalData: vaultEnvelopeAad() },
      kek,
      dashboardBase64UrlToBytes(String(envelope.wrapped || ''))
    );
    return new Uint8Array(kBytes);
  } catch {
    return null;
  }
}

async function vaultMasterAesKey(kBytes) {
  return crypto.subtle.importKey('raw', kBytes, { name: 'AES-GCM' }, false, ['encrypt', 'decrypt']);
}

async function vaultEncryptBody(kBytes, bodyObj, revision) {
  const key = await vaultMasterAesKey(kBytes);
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const ct = await crypto.subtle.encrypt(
    { name: 'AES-GCM', iv, additionalData: vaultBodyAad(revision) },
    key,
    new TextEncoder().encode(JSON.stringify(bodyObj))
  );
  return {
    iv: dashboardBytesToBase64Url(iv),
    ct: dashboardBytesToBase64Url(new Uint8Array(ct)),
  };
}

async function vaultDecryptBody(kBytes, blob) {
  try {
    const key = await vaultMasterAesKey(kBytes);
    const plaintext = await crypto.subtle.decrypt(
      {
        name: 'AES-GCM',
        iv: dashboardBase64UrlToBytes(String(blob?.body?.iv || '')),
        additionalData: vaultBodyAad(Number(blob?.revision) || 0),
      },
      key,
      dashboardBase64UrlToBytes(String(blob?.body?.ct || ''))
    );
    return JSON.parse(new TextDecoder().decode(plaintext));
  } catch {
    return null;
  }
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
   Canonical JSON (sorted keys) because the store's serializer is free to
   reorder object keys in transit. */

function vaultCanonicalJson(value) {
  if (Array.isArray(value)) return `[${value.map(vaultCanonicalJson).join(',')}]`;
  if (value && typeof value === 'object') {
    const keys = Object.keys(value).sort();
    return `{${keys.map(k => `${JSON.stringify(k)}:${vaultCanonicalJson(value[k])}`).join(',')}}`;
  }
  return JSON.stringify(value);
}

async function vaultMacKey(kBytes) {
  const hkdf = await crypto.subtle.importKey('raw', kBytes, 'HKDF', false, ['deriveKey']);
  return crypto.subtle.deriveKey(
    {
      name: 'HKDF',
      hash: 'SHA-256',
      salt: new TextEncoder().encode(VAULT_HKDF_SALT),
      info: new TextEncoder().encode('vault-mac-v1'),
    },
    hkdf,
    { name: 'HMAC', hash: 'SHA-256' },
    false,
    ['sign', 'verify']
  );
}

function vaultMacPayload(blob) {
  return new TextEncoder().encode(
    `intendant-vault-mac-v1\n${Number(blob.v) || 0}\n${String(blob.kind || '')}\n` +
    `${Number(blob.revision) || 0}\n${vaultCanonicalJson(blob.envelopes || [])}\n` +
    `${vaultCanonicalJson(blob.body || {})}`
  );
}

async function vaultComputeMac(kBytes, blob) {
  const key = await vaultMacKey(kBytes);
  const mac = await crypto.subtle.sign('HMAC', key, vaultMacPayload(blob));
  return dashboardBytesToBase64Url(new Uint8Array(mac));
}

async function vaultVerifyMac(kBytes, blob) {
  const mac = String(blob?.mac || '');
  if (!mac) return false;
  try {
    const key = await vaultMacKey(kBytes);
    return await crypto.subtle.verify(
      'HMAC', key, dashboardBase64UrlToBytes(mac), vaultMacPayload(blob)
    );
  } catch {
    return false;
  }
}

/* Once this device has seen an authenticated blob, an unauthenticated one
   is a downgrade attack, not a legacy vault. Ratchet state rides in
   vaultState and persists through vaultWriteLocal. */
function vaultMarkMacSeen() {
  if (!vaultState.macSeen) {
    vaultState.macSeen = true;
    vaultWriteLocal();
  }
}

/* ── Local cache + hosted sync ── */

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
  const resp = await fetch(accessFleetHostedUrl('/api/vault'));
  if (resp.status === 401) return { authenticated: false };
  const body = await resp.json().catch(() => ({}));
  if (!resp.ok || body.ok === false) throw new Error(body.error || `HTTP ${resp.status}`);
  return { authenticated: true, revision: Number(body.revision) || 0, vault: body.vault || null };
}

async function vaultServerPublish(blob) {
  const headers = await accessFleetHostedHeaders();
  if (!headers) throw new Error('sign in to the hosted account first');
  const resp = await fetch(accessFleetHostedUrl('/api/vault'), {
    method: 'POST',
    headers,
    body: JSON.stringify({ revision: blob.revision, vault: blob }),
  });
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
      ? `The hosted store returned vault revision ${revision}, but this device has already seen revision ${vaultState.highWater}. The store cannot read or forge the vault, but it can withhold updates — treat its copy as stale.`
      : '';
  }
  if (revision < vaultState.revision) return;
  // Authenticate before adopting. Unlocked: verify the MAC outright.
  // Locked with a MAC: adopt provisionally — unlock verifies before any
  // use. Either way the downgrade ratchet refuses an unauthenticated
  // blob once this device has seen an authenticated one.
  if (blob.mac && vaultState.kBytes) {
    if (!(await vaultVerifyMac(vaultState.kBytes, blob))) {
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
  if (vaultState.kBytes) {
    const body = await vaultDecryptBody(vaultState.kBytes, blob);
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
  if (vaultState.kBytes) vaultState.kBytes.fill(0);
  vaultState.kBytes = null;
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

async function vaultFinishUnlock(kBytes, envelopeId) {
  // Every unlock funnels through here: authenticate the whole blob before
  // trusting any of it. A blob without a MAC is legacy — allowed only
  // until this device has seen an authenticated one, and upgraded in
  // place right after unlock.
  if (vaultState.blob?.mac) {
    if (!(await vaultVerifyMac(kBytes, vaultState.blob))) {
      vaultState.lastError = 'Vault integrity check failed — the stored blob was tampered with or spliced. Refusing to unlock it.';
      renderAccessVaultSection();
      return false;
    }
    vaultMarkMacSeen();
  } else if (vaultState.macSeen) {
    vaultState.lastError = 'The store served an unauthenticated vault although this device has seen an authenticated one — refusing the downgrade.';
    renderAccessVaultSection();
    return false;
  }
  const body = await vaultDecryptBody(kBytes, vaultState.blob);
  if (!body) {
    vaultState.lastError = 'The key envelope opened but the vault body did not decrypt — the blob may be corrupted.';
    return false;
  }
  vaultState.kBytes = kBytes;
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
  return true;
}

async function vaultTryPrfUnlock({ silent = false } = {}) {
  if (!vaultState.blob) return false;
  let sawKek = false;
  for (const envelope of vaultState.blob.envelopes || []) {
    if (envelope.kind !== 'prf') continue;
    // Each envelope names its PRF domain: marked = dedicated vault salt,
    // markerless = legacy fleet-secret derivation.
    const kek = await vaultPrfKekForEnvelope(envelope);
    if (!kek) continue;
    sawKek = true;
    const kBytes = await vaultUnwrapMasterKey(kek, envelope);
    if (kBytes) {
      const unlocked = await vaultFinishUnlock(kBytes, envelope.id);
      if (unlocked && envelope.prf !== VAULT_PRF_ENVELOPE_MARK) {
        vaultMigratePrfEnvelope(envelope.id);
      }
      return unlocked;
    }
  }
  if (!silent) {
    vaultState.lastError = sawKek
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
      if (!vaultState.kBytes || !vaultState.blob) return false;
      const [kek, mark] = await vaultPrfKekForWrap();
      if (!kek || mark !== VAULT_PRF_ENVELOPE_MARK) return false;
      const envelopes = [];
      let migrated = false;
      for (const envelope of vaultState.blob.envelopes || []) {
        if (envelope.kind === 'prf' && envelope.id === envelopeId && !envelope.prf) {
          envelopes.push({
            ...envelope,
            prf: VAULT_PRF_ENVELOPE_MARK,
            ...(await vaultWrapMasterKey(kek, vaultState.kBytes)),
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
  const kek = await vaultPhraseKek(phrase);
  for (const envelope of vaultState.blob?.envelopes || []) {
    if (envelope.kind !== 'phrase') continue;
    const kBytes = await vaultUnwrapMasterKey(kek, envelope);
    // matchedEnvelopeId means "the prf envelope this session's passkey
    // opened" — a phrase unlock matches none, which is exactly what
    // makes the enroll-this-passkey offer appear.
    if (kBytes) return vaultFinishUnlock(kBytes, null);
  }
  vaultState.lastError = 'The phrase is well-formed but does not open this vault.';
  renderAccessVaultSection();
  return false;
}

async function vaultCreate(phrase) {
  if (!vaultAvailable()) throw new Error('the vault needs a hosted session');
  const kBytes = crypto.getRandomValues(new Uint8Array(32));
  const now = Date.now();
  const envelopes = [];
  envelopes.push({
    kind: 'phrase',
    id: vaultRandomId('env'),
    label: 'Recovery phrase',
    created_unix_ms: now,
    ...(await vaultWrapMasterKey(await vaultPhraseKek(phrase), kBytes)),
  });
  let matched = null;
  const [prfKek, prfMark] = await vaultPrfKekForWrap();
  if (prfKek) {
    const envelope = {
      kind: 'prf',
      id: vaultRandomId('env'),
      label: `Passkey enrolled ${new Date(now).toISOString().slice(0, 10)}`,
      created_unix_ms: now,
      ...(prfMark ? { prf: prfMark } : {}),
      ...(await vaultWrapMasterKey(prfKek, kBytes)),
    };
    envelopes.push(envelope);
    matched = envelope.id;
  }
  const revision = Math.max(1, vaultState.highWater + 1);
  const blob = {
    v: 1,
    kind: 'intendant-vault',
    revision,
    created_unix_ms: now,
    updated_unix_ms: now,
    envelopes,
    body: await vaultEncryptBody(kBytes, { entries: [], settings: {} }, revision),
  };
  blob.mac = await vaultComputeMac(kBytes, blob);
  await vaultServerPublish(blob);
  vaultState.blob = blob;
  vaultState.revision = revision;
  vaultState.highWater = Math.max(vaultState.highWater, revision);
  vaultState.macSeen = true;
  vaultWriteLocal();
  await vaultFinishUnlock(kBytes, matched);
}

/* Re-encrypt and publish the unlocked state as the next revision. On a
   revision conflict: refetch, merge entries by (id, updated_unix_ms) —
   a concurrent update wins over a concurrent delete, never silently
   dropping a credential — and retry once. */
async function vaultPersist() {
  if (!vaultState.kBytes || !vaultState.blob) throw new Error('vault is locked');
  const attempt = async () => {
    const revision = Math.max(vaultState.revision, vaultState.highWater) + 1;
    const blob = {
      ...vaultState.blob,
      revision,
      updated_unix_ms: Date.now(),
      body: await vaultEncryptBody(
        vaultState.kBytes,
        { entries: vaultState.entries, settings: vaultState.settings },
        revision
      ),
    };
    // Recompute — the spread carries the previous revision's MAC.
    blob.mac = await vaultComputeMac(vaultState.kBytes, blob);
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
        ? await vaultVerifyMac(vaultState.kBytes, server.vault)
        : !vaultState.macSeen;
      if (!remoteAuthentic) {
        vaultState.lastError = 'The conflict refetch returned a vault blob that failed its integrity check — keeping local state.';
        renderAccessVaultSection();
        throw err;
      }
      if (server.vault.mac) vaultMarkMacSeen();
      const remoteBody = await vaultDecryptBody(vaultState.kBytes, server.vault);
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

async function vaultEnrollThisPasskey() {
  if (!vaultState.kBytes) return;
  vaultState.lastError = '';
  try {
    let [kek, mark] = await vaultPrfKekForWrap();
    if (!kek) {
      if (!(await vaultRequestPrfSecret())) throw new Error('this authenticator did not return a PRF secret');
      [kek, mark] = await vaultPrfKekForWrap();
    }
    if (!kek) throw new Error('no PRF secret available');
    for (const envelope of vaultState.blob.envelopes || []) {
      if (envelope.kind !== 'prf') continue;
      const envelopeKek = await vaultPrfKekForEnvelope(envelope);
      if (envelopeKek && (await vaultUnwrapMasterKey(envelopeKek, envelope))) {
        vaultState.matchedEnvelopeId = envelope.id;
        renderAccessVaultSection();
        return;
      }
    }
    const now = Date.now();
    const envelope = {
      kind: 'prf',
      id: vaultRandomId('env'),
      label: `Passkey enrolled ${new Date(now).toISOString().slice(0, 10)}`,
      created_unix_ms: now,
      ...(mark ? { prf: mark } : {}),
      ...(await vaultWrapMasterKey(kek, vaultState.kBytes)),
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
  if (vaultState.status === 'unlocked' || vaultState.kBytes) {
    for (const [storageKey, provider] of Object.entries(VAULT_VOICE_STORAGE_PROVIDERS)) {
      const entry =
        vaultState.entries.find(e => e.kind === 'api_key' && e.provider === provider && e.voice && e.secret) ||
        vaultState.entries.find(e => e.kind === 'api_key' && e.provider === provider && e.secret);
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
  supported: null,   // null until first probe; false when the daemon lacks the RPCs / gate
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

function vaultLeaseTransportReady() {
  if (!DASHBOARD_CONNECT_MODE) return false;
  try {
    const status = window.intendantDashboardControl?.status?.();
    if (!status?.connected || !status?.verifiedBinding?.ok) return false;
    // Leases need the credential RPC family. A daemon that advertises its
    // control surface but not these methods is too old — report
    // unsupported instead of firing calls that fail one by one. An empty
    // list means the hello_ack hasn't landed yet: fall through and let
    // the RPCs answer.
    const features = status.controlFeatures;
    if (Array.isArray(features) && features.length) {
      return features.includes('api_credential_lease_status');
    }
    return true;
  } catch {
    return false;
  }
}

function vaultLeaseRpc(method, params = {}) {
  return window.intendantDashboardControl.request(method, params, { timeoutMs: 15000 });
}

/* The lease kind a vault entry fuels, or null when it cannot fuel. */
function vaultEntryLeaseKind(entry) {
  if (!entry || !entry.secret) return null;
  if (entry.kind === 'api_key' && ['anthropic', 'openai', 'gemini'].includes(entry.provider)) {
    return `api_key:${entry.provider}`;
  }
  if (entry.kind === 'oauth' && ['codex', 'claude-code'].includes(entry.provider)) {
    return `oauth:${entry.provider}`;
  }
  return null;
}

async function vaultRefreshLeases({ force = false } = {}) {
  if (!vaultLeaseTransportReady()) return;
  if (!force && Date.now() - vaultLeaseState.fetchedAt < 30_000) return;
  try {
    const result = await vaultLeaseRpc('api_credential_lease_status');
    vaultLeaseState.supported = true;
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
    const message = String(err?.message || err);
    // A daemon predating the lease RPCs, or a session without the
    // credentials.manage gate, reads as unsupported rather than broken.
    vaultLeaseState.supported = false;
    vaultLeaseState.lastError = message;
    vaultLeaseState.fetchedAt = Date.now();
  }
  renderAccessVaultSection();
  refreshUnfueledEmptyState().catch(() => {});
  vaultRefreshCustody().catch(() => {});
}

/* Custody trail: the daemon's own record of lease/relay lifecycle events,
   fetched over the same credentials.manage-gated channel as the leases. */
const custodyTrailState = { events: [], supported: null, fetchedAt: 0 };

async function vaultRefreshCustody({ force = false } = {}) {
  if (!vaultLeaseTransportReady()) return;
  if (!force && Date.now() - custodyTrailState.fetchedAt < 30_000) return;
  custodyTrailState.fetchedAt = Date.now();
  try {
    const result = await vaultLeaseRpc('api_credential_custody_trail');
    custodyTrailState.events = Array.isArray(result?.events) ? result.events : [];
    custodyTrailState.supported = true;
  } catch {
    // Older daemon or a session without credentials.manage.
    custodyTrailState.supported = false;
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
    note('This session cannot read the custody trail — it needs credentials.manage, or the daemon predates it.');
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
    meta.textContent = [event.actor ? `by ${event.actor}` : '', event.detail]
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
  return (
    vaultState.entries.find(e => e.kind === 'api_key' && e.provider === provider && e.secret && !e.voice) ||
    vaultState.entries.find(e => e.kind === 'api_key' && e.provider === provider && e.secret) ||
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

/* ── Vault UI (Access → Advanced) ── */

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
    const secret = document.createElement('span');
    secret.className = 'secret';
    const secretValue = String(entry.secret || '');
    secret.textContent = vaultRevealedEntries.has(entry.id)
      ? secretValue || '(token set)'
      : secretValue
        ? `••••${secretValue.slice(-4)}`
        : '(token set)';
    const actions = document.createElement('span');
    actions.className = 'vault-entry-actions';
    if (secretValue) {
      actions.appendChild(
        vaultButton(vaultRevealedEntries.has(entry.id) ? 'Hide' : 'Reveal', () => {
          if (vaultRevealedEntries.has(entry.id)) vaultRevealedEntries.delete(entry.id);
          else vaultRevealedEntries.add(entry.id);
          renderAccessVaultSection();
        })
      );
      actions.appendChild(
        vaultButton('Copy', () => {
          navigator.clipboard?.writeText(secretValue).catch(() => {});
        })
      );
    }
    actions.appendChild(
      vaultButton('Remove', () => {
        vaultRemoveEntry(entry.id);
        renderAccessVaultSection();
      }, { danger: true })
    );
    row.append(lbl, chip, kindChip, secret, actions);
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
  grid.append(kindLabel, kindSelect, providerLabel, providerSelect, labelLabel, labelInput, secretLabel, secretInput);
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
   fuelable vault entry. Rendered only in connect mode. */
function vaultRenderFueling(card) {
  if (!DASHBOARD_CONNECT_MODE) return;
  const head = document.createElement('div');
  head.className = 'vault-status-line';
  const title = document.createElement('span');
  title.className = 'lbl';
  title.style.fontWeight = '600';
  title.textContent = 'Fueling — this daemon';
  head.appendChild(title);
  card.appendChild(head);

  if (!vaultLeaseTransportReady()) {
    const note = document.createElement('div');
    note.className = 'vault-note';
    note.textContent = 'Connect to the daemon to fuel it — leases travel only over the verified tunnel.';
    card.appendChild(note);
    return;
  }
  vaultRefreshLeases().catch(() => {});
  if (vaultLeaseState.supported === false) {
    const note = document.createElement('div');
    note.className = 'vault-note';
    note.textContent = `This session cannot manage credential leases: ${vaultLeaseState.lastError || 'the daemon predates leases or this role lacks credentials.manage.'}`;
    card.appendChild(note);
    return;
  }

  if (vaultLeaseState.expiredNote) {
    const warning = document.createElement('div');
    warning.className = 'vault-warning';
    warning.textContent = vaultLeaseState.expiredNote;
    card.appendChild(warning);
  }
  if (vaultLeaseState.lastError && vaultLeaseState.supported) {
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
      statusText = DASHBOARD_CONNECT_MODE
        ? 'This browser lacks the WebCrypto features the vault needs.'
        : 'The vault rides your hosted account — open this dashboard through Hosted Connect to use it.';
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
  statusLine.append(chip, document.createTextNode(statusText));
  card.appendChild(statusLine);

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
      const applied = await accessOrgCall('api_access_org_orl_apply', '/api/access/orgs/revocations/apply', body.orl);
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

function dashboardJsonFetch(method, params, fallback, label = method, options = {}) {
  if (dashboardTransport && typeof dashboardTransport.jsonFetch === 'function') {
    return dashboardTransport.jsonFetch(method, params, fallback, label, options);
  }
  return fallback();
}

function dispatchSessionControlMsg(payload, options = {}) {
  const action = String(payload?.action || '').trim();
  const fallback = () => {
    if (dashboardConnectModeEnabled()) {
      return dashboardConnectMutationUnavailable(action, 'Session control request', options);
    }
    if (app && app.send_server_action) {
      app.send_server_action(payload);
      return true;
    }
    console.warn('session control: no app connection, dropped', payload);
    return false;
  };
  if (!DASHBOARD_SESSION_CONTROL_MSG_RPC_ACTIONS.has(action)) return fallback();
  if (
    dashboardTransport &&
    dashboardTransport.canUseRpc &&
    dashboardTransport.canUseRpc() &&
    dashboardControlTransport?.lastStatus?.api_session_control_msg_available === true
  ) {
    dashboardTransport.request('api_session_control_msg', { message: payload }, {
      timeoutMs: options.timeoutMs || 15000,
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
      app.send_server_action(payload);
      return true;
    }
    console.warn('dashboard action: no app connection, dropped', payload);
    return false;
  };
  if (!DASHBOARD_ACTION_MSG_RPC_ACTIONS.has(action)) return fallback();
  if (
    dashboardTransport &&
    dashboardTransport.canUseRpc &&
    dashboardTransport.canUseRpc() &&
    dashboardControlTransport?.lastStatus?.api_dashboard_action_msg_available === true
  ) {
    dashboardTransport.request('api_dashboard_action_msg', { message: payload }, {
      timeoutMs: options.timeoutMs || 15000,
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

