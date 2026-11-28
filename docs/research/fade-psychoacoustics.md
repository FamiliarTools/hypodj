# Fade research backfill (task t:4iiglbm)

Scope: psychoacoustics of fade curves and equal loudness, and incompleteness/Zeigarnik distress for fades mid-track vs at boundaries. Compared against crates/hypodj-core/src/fade.rs (Curve::DbLinear only, sub-JND step 0.75 dB, deliberate cap 3 dB, min slew 200 ms hard floor, floor -60 dB then mute, low_floor default -45 dB) and memory m:01kxhjqg (fade design spec).

Method note: web search returned mostly secondary summaries. Items marked (secondary) were not read in the original paper. Nothing below was found as a direct AES fade-curve listening test; that gap is stated, not papered over.

## 1. Psychoacoustics

### 1.1 Equal loudness, ISO 226
- ISO 226:2023 (3rd ed., 2023-03) replaces 2003. Contours are nearly identical to 2003 (differences under ~0.5 dB over 20-90 phon per Wikipedia summary; the revision changed the power exponent and the 20 Hz threshold after ISO 389-7:2019). Numeric data for one-third-octave 20 Hz-12.5 kHz; formulas reliable roughly 20-90 phon (20 Hz-4 kHz).
  - https://www.iso.org/standard/83117.html
  - Suzuki et al., Acoust. Sci. & Tech. 45(1), doi:10.1250/ast.e23.66, https://www.jstage.jst.go.jp/article/ast/45/1/45_e23.66/_pdf/-char/en
- Consequence I derive (standard knowledge, not quoted from the search results): contours are not parallel. Spacing narrows at low frequencies, so lowering overall level by X dB lowers perceived bass loudness by more than mid loudness. A flat-gain fade therefore sounds like it loses bass first and goes "thin" before it goes silent. The fade is spectrally neutral in dB but not in phon.
- Also, contours are defined for pure tones in free field at 20+ phon; the -60 dB floor tail lies outside the validated region, so the standard says nothing about the last ~30 dB of the ramp.

### 1.2 Level JND
- Intensity JND for broadband noise is near-constant in dB, roughly 0.5-1 dB (Weber behavior); tones show the "near miss" (slope ~0.9) which broadband masking removes.
  - McGill and Goldberg 1968 via https://www.researchgate.net/publication/10758571_Recasting_the_Near-Miss_to_Weber's_Law
  - https://www.researchgate.net/publication/258027956_Modeling_the_relation_between_the_intensity_just-noticeable_difference_and_loudness_for_pure_tones_and_wideband_noise
- Supports the 0.75 dB sub-JND step. Caveat: JND is for discriminating two static levels. A fade is a continuous ramp, where detection depends on rate (dB/s), so a "sub-JND step" is a sufficient but not necessary condition for smoothness; the interpolation inside the audio path already makes steps moot.

### 1.3 Fade curve shape perception
- Springer APP 2023, "Distinguishing between straight and curved sounds: Auditory shape in pitch, loudness, and tempo gestures": linear amplitude loudness gestures varied most in perceived shape; exponential amplitude (linear in dB) was heard as exponential; raised-cosine and linear amplitude came close to the straight-line reference. https://link.springer.com/article/10.3758/s13414-023-02764-8
  - Reading: "linear in dB" is NOT perceived as a straight line. Perceived shape and perceived rate are different things, and the paper is about shape judgment, not smoothness or startle.
- Puckette, Theory and Technique of Electronic Music: a linear amplitude fade is not perceptually smooth; ideal lies between log and linear; a pure constant-dB-rate ramp has its own flaw (most of the audible change is crowded where the signal is already quiet, or the first half of a 10 s fade spans ~50 dB of the 100 dB range). https://msp.ucsd.edu/techniques/v0.11/book-html/node60.html
- Rule of thumb in the practical literature: ~10 dB attenuation ~ half loudness (Stevens sone scale). This agrees with the code comment (loudness compressive, +10 dB ~ 2x). Note the code comment says Stevens ~0.67; that is the sound-pressure exponent (0.67 on pressure = ~0.3 on intensity = doubling per 10 dB). Consistent, just worth knowing which variable.
- Informal engineer consensus (KDE forum, weak): "straight in dB sounds steady". https://discuss.kde.org/t/audio-fades-equal-power-and-logarithmic-using-avfilter-afade/40291
- GAP: no AES or JASA paper found that compares fade curves for preferred or least-noticeable fade-outs. Searching the AES E-Library directly (terms "crossfade law", "fade curve perception") is the next step if this matters; I could not reach it.

### 1.4 Crossfade law
- Equal-power (constant power, sin/cos) for uncorrelated material, equal-gain for correlated; the hypodj memo already specifies equal-power for crossfades. Nothing found contradicts this. (Standard DSP practice; Puckette link above covers amplitude shapes.)

## 2. Incompleteness, Zeigarnik, OCD

### 2.1 Zeigarnik is weak, Ovsiankina is strong
- Ghibellini and Meier 2025, "Interruption, recall and resumption: A meta-analysis of the Zeigarnik and Ovsiankina effects", Humanities and Social Sciences Communications 12, 962. (secondary: summaries of the paper) Pooled recall ratio interrupted:completed = 0.99 (d = 0.15); no memory advantage for unfinished tasks. People resumed interrupted tasks in ~67% of cases across 21 publications (Ovsiankina effect holds).
  - https://www.nature.com/articles/s41599-025-05000-w (paywalled redirect; not read directly)
  - https://yukaichou.com/behavioral-analysis/zeigarnik-effect-incomplete-tasks-memory-tension/ (secondary)
- Wendsche et al. 2026 (secondary, via search summary): unfinished work tasks linked to off-job work thoughts, rho = .38 between persons, 17 studies, 2473 workers. Context is work tasks, not music.
- Meaning for hypodj: the "cognitive memory of unfinished" claim should not be a driver. The surviving effect is the urge to RESUME, and a state of unresolved tension. Plan-making reportedly reduces intrusions (secondary, Masicampo and Baumeister line, not verified here).

### 2.2 OCD: not-just-right experiences (NJRE) and incompleteness
- NJREs: discomfort that things are not "just right"; Coles, Frost, Heimberg, Rheaume 2003; Ghisi et al. 2010 (https://www.sciencedirect.com/science/article/abs/pii/S0887618510001374). Incompleteness and NJRE predict OCD symptoms beyond dysfunctional beliefs; ~65% of OCD patients report NJREs, experienced more as discomfort than anxiety; CBT reduces them (https://www.researchgate.net/publication/289573400_Incompleteness_and_not_just_right_experiences_in_the_explanation_of_Obsessive-Compulsive_Disorder).
- Auditory NJRE is real: Summers, Fitch, Cougle 2014, Behavior Therapy, "Visual, tactile, and auditory not just right experiences" used an out-of-tune music clip as the auditory trigger in 284 undergraduates; discomfort and urge to counteract correlated with OC symptoms (ordering, checking, washing) and perfectionism. https://www.sciencedirect.com/science/article/abs/pii/S0005789414000574
- 1001-patient study of sensory phenomena (Miguel et al.) lists auditory just-right perceptions. https://www.researchgate.net/publication/221857803
- Induced NJRE and incompleteness in vivo: correlate with compulsion severity, ordering, washing, hoarding. https://www.sciencedirect.com/science/article/abs/pii/S000579161630221X
- NOT FOUND: any study of music interrupted mid-song, or fade vs boundary, in OCD or anxiety. The inference "a fade mid-song causes completion distress" is extrapolation from NJRE (a stimulus that violates the subjective criterion triggers an urge to fix it) and from music cognition.

### 2.3 Music-cognition side: tension from lack of closure
- Margulis, "Silences in music are musical not silent" (Music Perception; secondary summary at https://musicpsychology.co.uk/the-music-of-silence/): a pause after music WITH closure gave lower reported tension, after music WITHOUT closure higher tension (non-musicians).
- Phrase-boundary EEG: expectation violations raise tension ratings and ERAN/N5 responses. https://www.ncbi.nlm.nih.gov/pmc/articles/PMC9117266/
- Meyer 1956 expectation theory, expectancy study https://arxiv.org/pdf/1708.03687 (pitch and timing predictability shape emotion).
- Reading: a gradual fade is a PREDICTABLE, slow, monotone change; the literature on violated expectation concerns abrupt or unresolved events. Fade length measured in minutes mostly removes surprise, but does not supply harmonic closure. Nothing here tests that.

## 3. Implications for hypodj

Evidence strength: S strong (primary, replicated), M moderate (primary single source or sound inference), W weak (secondary or extrapolated).

1. Keep dB-linear default and the 0.75 dB step. Evidence S for dB-domain level perception and ~1 dB JND. No change.
2. Optional ease-out tail (the spec says "optional" and code has only Curve::DbLinear, #[non_exhaustive]). Evidence W/M: Puckette argues pure log has crowding issues; Springer study says dB-linear is heard as exponential-shaped. Change if wanted: add a Curve variant (e.g. cosine-in-dB ease, slower near the floor) behind config, default stays DbLinear. Not justified by evidence strong enough to change defaults. Low priority.
3. Tail below ~-45 dB: ISO 226 is silent here, and bass loses loudness faster than mids as level drops (M, from contour spacing). The last 15 dB of a -60 dB floor ramp is perceived mostly as high-mid content. Optional: spend more time between -30 and -50 dB if the goal is "gentle", or accept. No hard change. Existing -45 dB wind-down floor is reasonable.
4. Equal-loudness-aware (low-shelf compensation) fading: inference only, M for the physics, W for the benefit; adds a filter in the mpv path against minimal-footprint principle. Recommend NOT doing it.
5. Mid-track vs boundary fades: no direct evidence that mid-song fade distresses OCD listeners (gap, not a negative result). Evidence for risk is W (NJRE auditory is real, music closure tension is real, Ovsiankina urge to resume is S in general populations). Cheap design hedge consistent with current architecture (envelope is content-agnostic, boundary-continuous): where the plan is "wind-down by time T" and a track boundary lies within a small tolerance window, optionally prefer completing the phrase or track and fading across the boundary. Make it a config knob (e.g. fade.prefer_boundary_within_s, default off), not a default change. Evidence W; only worth it if Guilherme experiences it as a problem.
6. Do not rely on Zeigarnik as a rationale in docs or code comments: the 2025 meta-analysis finds no memory effect (S). Use "resumption urge" and "NJRE/closure tension" wording if the topic is cited. Doc-only change.
7. The "Stevens ~0.67" comment in fade.rs line 20: accurate for sound pressure; add "on pressure" to avoid confusion with the 0.3 intensity exponent. Trivial doc fix, S.
8. Predictability and gradual rate stay the primary mitigation (cancelable, scheduled, slow). This matches the one thing the closure literature does support (violation of expectation is what raises tension). Evidence M. No change.

Net: no evidence found that requires changing current fade behavior. Two doc fixes (items 6, 7), one optional curve variant (2), one optional boundary-preference knob (5).
