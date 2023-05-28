use bc_crypto::RandomNumberGenerator;
use bc_shamir::{split_secret, recover_secret};
use crate::{Error, SSKRShare, METADATA_LENGTH_BYTES, Secret, Spec};

fn serialize_share(share: &SSKRShare) -> Vec<u8> {
    // pack the id, group and member data into 5 bytes:
    // 76543210        76543210        76543210
    //         76543210        76543210
    // ----------------====----====----====----
    // identifier: 16
    //                 group-threshold: 4
    //                     group-count: 4
    //                         group-index: 4
    //                             member-threshold: 4
    //                                 reserved (MUST be zero): 4
    //                                     member-index: 4

    let mut result = Vec::with_capacity(share.value().len() + METADATA_LENGTH_BYTES);
    let id = share.identifier();
    let gt = (share.group_threshold() - 1) & 0xf;
    let gc = (share.group_count() - 1) & 0xf;
    let gi = share.group_index() & 0xf;
    let mt = (share.member_threshold() - 1) & 0xf;
    let mi = share.member_index() & 0xf;

    let id1 = id >> 8;
    let id2 = id & 0xff;

    result.push(id1 as u8);
    result.push(id2 as u8);
    result.push(((gt << 4) | gc) as u8);
    result.push(((gi << 4) | mt) as u8);
    result.push(mi as u8);
    result.extend_from_slice(share.value().data());

    result
}

fn deserialize_share(source: &[u8]) -> Result<SSKRShare, Error> {
    if source.len() < METADATA_LENGTH_BYTES {
        return Err(Error::NotEnoughSerializedBytes);
    }

    let group_threshold = ((source[2] >> 4) + 1) as usize;
    let group_count = ((source[2] & 0xf) + 1) as usize;

    if group_threshold > group_count {
        return Err(Error::InvalidGroupThreshold);
    }

    let identifier = ((source[0] as u16) << 8) | source[1] as u16;
    let group_index = (source[3] >> 4) as usize;
    let member_threshold = ((source[3] & 0xf) + 1) as usize;
    let reserved = source[4] >> 4;
    if reserved != 0 {
        return Err(Error::InvalidReservedBits);
    }
    let member_index = (source[4] & 0xf) as usize;
    let value = Secret::new(source[METADATA_LENGTH_BYTES..].to_vec())?;

    Ok(SSKRShare::new(
        identifier,
        group_index,
        group_threshold,
        group_count,
        member_index,
        member_threshold,
        value,
    ))
}

fn generate_shares(
    spec: &Spec,
    master_secret: &Secret,
    random_generator: &mut impl RandomNumberGenerator
) -> Result<Vec<Vec<SSKRShare>>, Error> {
    // assign a random identifier
    let mut identifier = [0u8; 2];
    random_generator.fill_random_data(&mut identifier);
    let identifier: u16 = ((identifier[0] as u16) << 8) | identifier[1] as u16;

    let mut groups_shares: Vec<Vec<SSKRShare>> = Vec::with_capacity(spec.group_count());

    let group_secrets = split_secret(spec.group_threshold(), spec.group_count(), master_secret.data(), random_generator).map_err(Error::ShamirError)?;

    for (group_index, group) in spec.groups().iter().enumerate() {
        let group_secret = &group_secrets[group_index];
        let member_secrets = split_secret(group.threshold(), group.count(), group_secret, random_generator)
            .map_err(Error::ShamirError)?
            .into_iter().map(Secret::new)
            .collect::<Result<Vec<Secret>, _>>()?;
        let member_sskr_shares: Vec<SSKRShare> = member_secrets.into_iter().enumerate().map(|(member_index, member_secret)| {
            SSKRShare::new(
                identifier,
                group_index,
                spec.group_threshold(),
                spec.group_count(),
                member_index,
                group.threshold(),
                member_secret,
            )
        }).collect();
        groups_shares.push(member_sskr_shares);
    }

    Ok(groups_shares)
}

pub fn sskr_generate_using(
    spec: &Spec,
    master_secret: &Secret,
    random_generator: &mut impl RandomNumberGenerator
) -> Result<Vec<Vec<Vec<u8>>>, Error> {
    let groups_shares = generate_shares(spec, master_secret, random_generator)?;

    let result: Vec<Vec<Vec<u8>>> = groups_shares.iter().map (|group| {
        group.iter().map(serialize_share).collect()
    }).collect();

    Ok(result)
}

pub fn sskr_generate(
    spec: &Spec,
    master_secret: &Secret
) -> Result<Vec<Vec<Vec<u8>>>, Error> {
    let mut rng = bc_crypto::SecureRandomNumberGenerator;
    sskr_generate_using(spec, master_secret, &mut rng)
}

#[derive(Debug)]
struct Group {
    group_index: usize,
    member_threshold: usize,
    count: usize,
    member_indexes: Vec<usize>,
    member_shares: Vec<Secret>,
}

impl Group {
    fn new(group_index: usize, member_threshold: usize) -> Self {
        Self {
            group_index,
            member_threshold,
            count: 0,
            member_indexes: Vec::with_capacity(16),
            member_shares: Vec::with_capacity(16),
        }
    }
}

fn combine_shares(shares: &[SSKRShare]) -> Result<Secret, Error> {
    let mut identifier = 0;
    let mut group_threshold = 0;
    let mut group_count = 0;

    if shares.is_empty() {
        return Err(Error::EmptyShareSet);
    }

    let mut next_group = 0;
    let mut groups: Vec<Group> = Vec::with_capacity(16);
    let mut secret_len = 0;

    for (i, share) in shares.iter().enumerate() {
        if i == 0 {
            // on the first one, establish expected values for common metadata
            identifier = share.identifier();
            group_count = share.group_count();
            group_threshold = share.group_threshold();
            secret_len = share.value().len();
        } else {
            // on subsequent shares, check that common metadata matches
            if share.identifier() != identifier ||
                share.group_threshold() != group_threshold ||
                share.group_count() != group_count ||
                share.value().len() != secret_len
            {
                return Err(Error::InvalidShareSet);
            }
        }

        // sort shares into member groups
        let mut group_found = false;
        for group in groups.iter_mut() {
            if share.group_index() == group.group_index {
                group_found = true;
                if share.member_threshold() != group.member_threshold {
                    return Err(Error::InvalidMemberThreshold);
                }
                for k in 0..group.count {
                    if share.member_index() == group.member_indexes[k] {
                        return Err(Error::DuplicateMemberIndex);
                    }
                }
                group.member_indexes.push(share.member_index());
                group.member_shares.push(share.value().clone());
            }
        }

        if !group_found {
            let mut g = Group::new(share.group_index(), share.member_threshold());
            g.member_indexes.push(share.member_index());
            g.member_shares.push(share.value().clone());
            groups.push(g);
            next_group += 1;
        }
    }

    if next_group < group_threshold {
        return Err(Error::NotEnoughGroups);
    }

    // here, all of the shares are unpacked into member groups. Now we go through each
    // group and recover the group secret, and then use the result to recover the
    // master secret
    let mut master_indexes = Vec::with_capacity(16);
    let mut master_shares = Vec::with_capacity(16);

    for group in groups {
        let group_secret = recover_secret(&group.member_indexes, &group.member_shares).map_err(Error::ShamirError)?;
        master_indexes.push(group.group_index);
        master_shares.push(group_secret);
    }

    let master_secret = recover_secret(&master_indexes, &master_shares).map_err(Error::ShamirError)?;
    let master_secret = Secret::new(master_secret)?;

    Ok(master_secret)
}

pub fn sskr_combine<T>(shares: &[T]) -> Result<Secret, Error>
where
    T: AsRef<[u8]>
{
    let mut sskr_shares = Vec::with_capacity(shares.len());

    for share in shares {
        let sskr_share = deserialize_share(share.as_ref())?;
        sskr_shares.push(sskr_share);
    }

    combine_shares(&sskr_shares)
}
