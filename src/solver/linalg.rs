pub fn solve_symmetric(a: &[f64], rhs: &[f64], n: usize) -> Option<Vec<f64>> {
    let mut matrix = a.to_vec();
    let mut b = rhs.to_vec();
    let mut pivots = vec![0usize; n];

    for col in 0..n {
        let mut best = col;
        for row in (col + 1)..n {
            if matrix[row * n + col].abs() > matrix[best * n + col].abs() {
                best = row;
            }
        }
        if !matrix[best * n + col].is_finite() || matrix[best * n + col].abs() < 1e-14 {
            return None;
        }
        if best != col {
            for k in 0..n {
                matrix.swap(col * n + k, best * n + k);
            }
            b.swap(col, best);
        }
        pivots[col] = best;
        let pivot = matrix[col * n + col];
        for row in (col + 1)..n {
            let factor = matrix[row * n + col] / pivot;
            matrix[row * n + col] = factor;
            for k in (col + 1)..n {
                matrix[row * n + k] -= factor * matrix[col * n + k];
            }
            b[row] -= factor * b[col];
        }
    }

    for col in (0..n).rev() {
        let mut value = b[col];
        for k in (col + 1)..n {
            value -= matrix[col * n + k] * b[k];
        }
        b[col] = value / matrix[col * n + col];
    }
    Some(b)
}

pub fn invert_symmetric(matrix: &[f64], n: usize) -> Option<Vec<f64>> {
    let mut inverse = vec![0.0; n * n];
    for column in 0..n {
        let mut identity = vec![0.0; n];
        identity[column] = 1.0;
        let solved = solve_symmetric(matrix, &identity, n)?;
        for row in 0..n {
            inverse[row * n + column] = solved[row];
        }
    }
    for i in 0..n {
        for j in (i + 1)..n {
            let mean = (inverse[i * n + j] + inverse[j * n + i]) * 0.5;
            inverse[i * n + j] = mean;
            inverse[j * n + i] = mean;
        }
    }
    Some(inverse)
}

pub fn diagonalize(mut matrix: Vec<f64>, n: usize) -> (Vec<f64>, Vec<f64>) {
    let mut vectors = vec![0.0; n * n];
    for i in 0..n {
        vectors[i * n + i] = 1.0;
    }
    for _ in 0..80 {
        let mut max = 0.0;
        let mut p = 0;
        let mut q = 0;
        for i in 0..n {
            for j in (i + 1)..n {
                let value = matrix[i * n + j].abs();
                if value > max {
                    max = value;
                    p = i;
                    q = j;
                }
            }
        }
        if max < 1e-13 {
            break;
        }
        let app = matrix[p * n + p];
        let aqq = matrix[q * n + q];
        let apq = matrix[p * n + q];
        let tau = (aqq - app) / (2.0 * apq);
        let t = if tau >= 0.0 {
            1.0 / (tau + (1.0 + tau * tau).sqrt())
        } else {
            -1.0 / (-tau + (1.0 + tau * tau).sqrt())
        };
        let c = 1.0 / (1.0 + t * t).sqrt();
        let s = t * c;
        for k in 0..n {
            let akp = matrix[k * n + p];
            let akq = matrix[k * n + q];
            matrix[k * n + p] = c * akp - s * akq;
            matrix[k * n + q] = s * akp + c * akq;
        }
        for k in 0..n {
            let apk = matrix[p * n + k];
            let aqk = matrix[q * n + k];
            matrix[p * n + k] = c * apk - s * aqk;
            matrix[q * n + k] = s * apk + c * aqk;
        }
        for k in 0..n {
            let vkp = vectors[k * n + p];
            let vkq = vectors[k * n + q];
            vectors[k * n + p] = c * vkp - s * vkq;
            vectors[k * n + q] = s * vkp + c * vkq;
        }
    }
    let values = (0..n).map(|i| matrix[i * n + i]).collect();
    (values, vectors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solves_and_inverts() {
        let a = vec![4.0, 1.0, 1.0, 3.0];
        let x = solve_symmetric(&a, &[1.0, 2.0], 2).unwrap();
        assert!((x[0] - 0.0909091).abs() < 1e-6);
        assert!((x[1] - 0.6363636).abs() < 1e-6);
        let inv = invert_symmetric(&a, 2).unwrap();
        assert!((inv[0] * 4.0 + inv[1] - 1.0).abs() < 1e-10);
    }
}
